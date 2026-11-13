//! Stage 2, reading documents.
//!
//! The worker itself reads nothing: it hands files to reader processes
//! (`readjob.rs`) in two lanes and saves what they return.
//!
//! - **What to read** is the catalog under the reading folders minus what
//!   the parts pool already covers; no file is opened to learn it was read.
//!   The stats thread works that out (`scan`) together with the counts the
//!   status bar shows.
//! - **Who reads it**: every PC that is on shares the backlog. Each takes
//!   the folders that fall to it first. The reader process takes a lease on
//!   a document before it opens it and leaves it to run out (half an hour)
//!   once it is read: long enough for the reading to be published and seen
//!   by every PC, so no document is read twice.
//! - **How hard**: the governor decides how many readers run, every pass
//!   of the loop. A busy PC keeps the minimum; a free one doubles every
//!   half minute.
//! - **Where it goes**: into `work.db` on this PC, published as a part of
//!   the office index every 500 documents or ten minutes.

use crate::pool::{self, Covered, Part, PathKeyer, path_key};
use crate::readjob::{Init, Job, Lane, Outcome, ReaderState};
use crate::service::{CountSnap, Engine, Item, Queue, Retry, fmt_count, now, under};
use crate::store::{Store, state};
use anyhow::{Context, Result};
use sleuth_core::kinds::FileKind;
use sleuth_core::paths::root_key;
use sleuth_core::protocol::{Failed, KindCount, ScopeProgress, SpeedMode};
use sleuth_lanes::{CpuMeter, Event, Governor, Inputs, ProcessGroup, Spawn, Speed, Workers};
use sleuth_office::{Acquire, Leases};

/// A document that could not be read is tried again after this long, once
/// nothing else waits.
const RETRY_FAILED_AFTER: i64 = 12 * 3600;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Readings are published once this many wait, or the oldest is this old.
const PUBLISH_DOCS: u64 = 500;
const PUBLISH_AFTER: Duration = Duration::from_secs(600);
/// How long one file may take. Generous: readers run at idle priority, so
/// on a PC that is flat out they get very little of the processor.
const FAST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const OCR_TIMEOUT: Duration = Duration::from_secs(90 * 60);
/// Waiting files kept ready per lane between scans.
const QUEUE_KEEP: usize = 3000;
/// Memory one OCR reader needs, for the governor's memory guard.
const OCR_WORKER_MB: u64 = 700;
/// Readings waiting to be published may take this much of the PC's disk
/// while the office folder is away.
const WORK_LIMIT: u64 = 200 * 1024 * 1024;

static MERGING: AtomicBool = AtomicBool::new(false);

fn is_local(path: &str) -> bool {
    !path.starts_with("\\\\")
}

fn split_path(path: &str) -> (String, String) {
    match path.rsplit_once('\\') {
        Some((dir, name)) => (if dir.len() == 2 && dir.ends_with(':') { format!("{dir}\\") } else { dir.to_string() }, name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

/// One of the two places unpublished readings wait: files on shares (to be
/// published to the office) and files on this PC's own disks.
struct Work {
    store: Store,
    part: Arc<Part>,
    path: PathBuf,
    unpublished: u64,
    since: Option<Instant>,
}

struct Coordinator {
    workers: Workers<Job, Outcome>,
    group: Arc<ProcessGroup>,
    office: Work,
    own: Work,
    /// Documents with a reader right now, by path key.
    flights: HashSet<u64>,
    gov: Governor,
    meter: CpuMeter,
    init_sent: String,
    /// When moving a 1.0 index into the office index was last tried.
    import_tried: Option<Instant>,
}

impl Engine {
    // ---- what is waiting -------------------------------------------------

    /// Counts for the status bar, and the head of the reading queue.
    pub(crate) fn stats_loop(self: Arc<Self>) {
        crate::sys::enter_background_mode();
        let mut fails_at = u64::MAX;
        while !self.stopping() {
            let started = Instant::now();
            let (mut snap, queue) = self.scan();
            // Failures are listed from the parts; only when those changed.
            if fails_at != self.pool.generation() {
                fails_at = self.pool.generation();
                let mut all: Vec<(String, String)> = self.pool.each(|_, s| s.failures(50)).into_iter().flat_map(|(_, f)| f).collect();
                all.truncate(50);
                snap.fails = all.into_iter().map(|(path, reason)| Failed { path, reason }).collect();
            } else {
                snap.fails = self.counts.lock().unwrap().fails.clone();
            }
            *self.counts.lock().unwrap() = snap;
            *self.queue.lock().unwrap() = queue;
            self.reader_signal.raise();
            // Never spend more than a tenth of the time counting.
            let spent = started.elapsed();
            self.stats_signal.wait((spent * 10).clamp(Duration::from_secs(3), Duration::from_secs(60)));
        }
    }

    /// Walk the catalog under the reading folders once: what is read, what
    /// failed, what waits, and the newest waiting files for each lane.
    fn scan(&self) -> (CountSnap, Queue) {
        let s = self.settings.read().unwrap().clone();
        let chosen = s.index_roots.clone().unwrap_or_else(|| s.content_scopes.clone());
        let drives = self.drives.read().unwrap().clone();
        let office: Vec<String> = self.office_roots();
        let readers = self.readers_now();
        let me = readers.iter().position(|p| *p == self.pc.to_lowercase()).unwrap_or(0) as u64;
        let t = now();
        let deferred: std::collections::HashSet<u64> = self.retry.lock().unwrap().iter().filter(|(_, r)| r.until > t).map(|(k, _)| *k).collect();
        let roots = self.roots.lock().unwrap().iter().map(|(k, r)| (k.clone(), r.state.clone().unwrap_or_default(), r.loaded)).collect::<Vec<_>>();
        let scopes: Vec<String> = self.read_scopes();
        let mut snap = CountSnap::default();
        let mut kinds: HashMap<FileKind, KindCount> = HashMap::new();
        let mut fast: Vec<(bool, i64, Item)> = Vec::new();
        let mut ocr: Vec<(bool, i64, Item)> = Vec::new();
        // Documents that could not be read, long enough ago to try again.
        let mut again: Vec<(bool, Item, bool)> = Vec::new();
        let trim = |v: &mut Vec<(bool, i64, Item)>| {
            if v.len() > QUEUE_KEEP * 2 {
                v.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
                v.truncate(QUEUE_KEEP);
            }
        };
        for folder in &chosen {
            let folder = folder.trim();
            if folder.is_empty() {
                continue;
            }
            let unc = crate::crawl::norm_dir(&self.stored_path(folder));
            let lower = unc.to_lowercase();
            let letter_unmapped = folder.len() >= 2 && folder.as_bytes()[1] == b':' && drives.drives.iter().all(|d| !d.letter.eq_ignore_ascii_case(&folder[..2]));
            // Counted towards the totals and queued only once: a folder
            // inside another chosen folder is already part of it.
            let nested = scopes.iter().any(|o| {
                let o = o.to_lowercase();
                o != lower && lower.starts_with(&format!("{}\\", o.trim_end_matches('\\')))
            });
            let mut sc = ScopeProgress { folder: folder.to_string(), unc: if letter_unmapped { String::new() } else { unc.clone() }, ..Default::default() };
            let mut keyer = PathKeyer::default();
            let mut dir_seen = String::new();
            let (mut dir_skip, mut dir_mine) = (false, true);
            self.catalog.for_each_under(&unc, &mut |dir, name, size, mtime| {
                let kind = FileKind::of(name);
                if !kind.readable() || s.kinds_not_read.contains(&kind) {
                    return true;
                }
                if dir != dir_seen {
                    dir_seen.clear();
                    dir_seen.push_str(dir);
                    let dl = dir.to_lowercase();
                    // Folders the office server reads are left to it.
                    dir_skip = under(&dl, &office);
                    dir_mine = readers.len() <= 1 || path_key(&dl) % readers.len() as u64 == me;
                }
                if dir_skip {
                    return true;
                }
                let key = keyer.key(dir, name);
                let at = self.pool.state_at_key(key, size, mtime);
                let st = at.map(|(s, _)| s);
                sc.pdfs += 1;
                let k = kinds.entry(kind).or_insert(KindCount { kind, found: 0, read: 0, waiting: 0, failed: 0 });
                if !nested {
                    k.found += 1;
                }
                match st {
                    Some(state::READ) => {
                        sc.read += 1;
                        if !nested {
                            k.read += 1;
                        }
                    }
                    Some(state::FAILED) => {
                        sc.failed += 1;
                        if !nested {
                            k.failed += 1;
                            // Tried again once nothing else waits (below).
                            if at.is_some_and(|(_, when)| when < t - RETRY_FAILED_AFTER) && !deferred.contains(&key) && again.len() < QUEUE_KEEP {
                                again.push((dir_mine, Item { path: crate::service::join(dir, name), size, mtime }, kind.needs_ocr()));
                            }
                        }
                    }
                    _ => {
                        sc.waiting += 1;
                        if nested {
                            return true;
                        }
                        k.waiting += 1;
                        let wants_ocr = st == Some(state::NEEDS_OCR) || kind.needs_ocr();
                        if wants_ocr {
                            snap.ocr_waiting += 1;
                        }
                        if !deferred.contains(&key) {
                            let item = Item { path: crate::service::join(dir, name), size, mtime };
                            let list = if wants_ocr { &mut ocr } else { &mut fast };
                            list.push((dir_mine, mtime, item));
                            trim(list);
                        }
                    }
                }
                true
            });
            let root = self.root_for(&unc).unwrap_or_default();
            let root_state = roots.iter().find(|(k, _, _)| *k == root);
            sc.note = if letter_unmapped {
                "not mapped"
            } else if root_state.is_some_and(|(_, st, _)| st.phase == "offline") {
                "offline"
            } else if sc.pdfs == 0 && root_state.is_none_or(|(_, st, loaded)| !loaded || st.last_ok == 0) {
                "not indexed yet"
            } else {
                "ok"
            }
            .into();
            if !nested {
                snap.read += sc.read;
                snap.waiting += sc.waiting;
                snap.failed += sc.failed;
            }
            snap.scopes.push(sc);
        }
        let mut kinds: Vec<KindCount> = kinds.into_values().collect();
        kinds.sort_by(|a, b| b.found.cmp(&a.found));
        snap.kinds = kinds;
        // Every reading folder done: what failed gets another go (a newer
        // reader, a file that was locked or half-copied, a share that dropped).
        // Each failure at most every RETRY_FAILED_AFTER; the document lease
        // keeps two PCs from trying the same one.
        if fast.is_empty() && ocr.is_empty() {
            for (mine, item, needs_ocr) in again {
                let mtime = item.mtime;
                if needs_ocr { ocr.push((mine, mtime, item)) } else { fast.push((mine, mtime, item)) }
            }
        }
        let ready = |mut v: Vec<(bool, i64, Item)>| -> VecDeque<Item> {
            // This PC's own folders first, newest files first.
            v.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
            v.truncate(QUEUE_KEEP);
            v.into_iter().map(|(_, _, i)| i).collect()
        };
        (snap, Queue { fast: ready(fast), ocr: ready(ocr) })
    }

    // ---- the reader loop ---------------------------------------------------

    /// Where a lane's readers come from: this program started again as a
    /// reader, or (tests, hosts that cannot do that) threads in this process.
    fn lane_spawn(&self, lane: Lane) -> Spawn<Job, Outcome> {
        let exe = std::env::var_os("SLEUTH_READER_EXE").map(PathBuf::from).or_else(|| {
            let exe = std::env::current_exe().ok()?;
            let name = exe.file_stem()?.to_string_lossy().to_lowercase();
            (std::env::var_os("SLEUTH_INPROC_READERS").is_none() && (name.starts_with("sleuth-worker") || name.starts_with("sleuth-server"))).then_some(exe)
        });
        match exe {
            Some(exe) => Spawn::Process { exe, args: vec!["--reader".into(), lane.name().into()], env: Vec::new() },
            None => Spawn::InProcess(Arc::new(|init: &serde_json::Value, job: Job| {
                thread_local! { static STATE: std::cell::RefCell<ReaderState> = std::cell::RefCell::new(ReaderState::default()); }
                match serde_json::from_value::<Init>(init.clone()) {
                    Ok(init) => STATE.with(|s| s.borrow_mut().handle(&init, job)),
                    Err(e) => Outcome::Failed { reason: format!("reader settings: {e}"), transient: true },
                }
            })),
        }
    }

    fn lane_init(&self, lane: Lane) -> serde_json::Value {
        let s = self.settings.read().unwrap().clone();
        let ocr_threads = std::env::var("SLEUTH_OCR_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(1usize);
        let init = Init { lane, settings: Box::new(s), cache_dir: self.store_dir().map(|d| d.to_string_lossy().into_owned()), ocr_threads, pc: self.pc.clone() };
        serde_json::to_value(init).unwrap_or_default()
    }

    fn open_work(&self, name: &str) -> Result<Work> {
        let path = self.data.join(name);
        let store = Store::open(&path).with_context(|| format!("open {}", path.display()))?;
        let part = self.pool.add_work(&path)?;
        let (read, other) = store.reading_counts()?;
        Ok(Work { store, part, path, unpublished: read + other, since: (read + other > 0).then(Instant::now) })
    }

    pub(crate) fn reader_loop(self: Arc<Self>) {
        crate::sys::enter_background_mode();
        let built = (|| -> Result<Coordinator> {
            let group = Arc::new(ProcessGroup::new()?);
            let mut workers: Workers<Job, Outcome> = Workers::new(Arc::clone(&group));
            for lane in [Lane::Fast, Lane::Ocr] {
                workers.add_lane(lane.name(), self.lane_spawn(lane), self.lane_init(lane));
            }
            Ok(Coordinator {
                workers,
                group,
                office: self.open_work("work.db")?,
                own: self.open_work("work-local.db")?,
                flights: HashSet::new(),
                gov: Governor::new(),
                meter: CpuMeter::new(),
                init_sent: String::new(),
                import_tried: None,
            })
        })();
        let mut c = match built {
            Ok(c) => c,
            Err(e) => {
                warn!("the reader cannot start: {e:#}");
                self.set_reader_state("stopped", None);
                self.set_reader_note(&format!("Reading could not start: {e:#}"));
                return;
            }
        };
        self.pool.recover(&|p| self.presence(p));
        while !self.stopping() {
            if let Err(e) = self.reader_pass(&mut c) {
                warn!("reader: {e:#}");
                self.reader_signal.wait(Duration::from_secs(5));
            }
        }
        // Stopping: give back what is in progress, publish what is done.
        c.workers.shutdown();
        self.publish(&mut c);
    }

    /// One turn of the loop: decide how many readers run, feed them, save
    /// what came back, publish when due.
    fn reader_pass(&self, c: &mut Coordinator) -> Result<()> {
        let s = self.settings.read().unwrap().clone();
        // Readers started from now on get the settings as they are now.
        let init = self.lane_init(Lane::Fast).to_string();
        if init != c.init_sent {
            c.init_sent = init;
            for lane in [Lane::Fast, Lane::Ocr] {
                c.workers.set_init(lane.name(), self.lane_init(lane));
            }
        }
        if self.work_dirty.swap(false, Ordering::SeqCst) {
            // A correction was written to `work.db` from another thread.
            let _gate = self.work_gate.lock().unwrap();
            for w in [&mut c.office, &mut c.own] {
                if let Ok((read, other)) = w.store.reading_counts() {
                    w.unpublished = read + other;
                    if w.unpublished > 0 {
                        w.since.get_or_insert_with(Instant::now);
                    }
                }
            }
        }
        // A Sleuth 1.0 index on this PC is moved into the office index as
        // soon as the office folder answers (and tried again hourly if that
        // did not finish).
        if c.import_tried.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) && self.store_dir().is_some() && self.data.join("index.db").exists() {
            c.import_tried = Some(Instant::now());
            self.import_old_index(c);
        }
        let urgent = !self.urgent.lock().unwrap().is_empty();
        // File names first: until the drives holding the reading folders
        // were listed to the end once, only files someone asked for are
        // read (unless told to read now).
        let first_walk = !self.read_now_flag.load(Ordering::SeqCst) && !self.reading_roots_listed();
        let store_away = self.store_dir().is_none();
        let work_full = std::fs::metadata(&c.office.path).map(|m| m.len()).unwrap_or(0) > WORK_LIMIT;
        let (fast_wait, ocr_wait) = {
            let q = self.queue.lock().unwrap();
            (!q.fast.is_empty(), !q.ocr.is_empty())
        };
        let backlog_allowed = !s.pause_reading && !first_walk && !(store_away && work_full);
        let speed = match self.speed() {
            SpeedMode::Auto => Speed::Auto,
            SpeedMode::Gentle => Speed::Gentle,
            SpeedMode::Full => Speed::Full,
        };
        let others = c.meter.others_percent(c.group.cpu_100ns());
        let mut t = c.gov.step(
            &Inputs {
                cores: sleuth_lanes::cores(),
                others_cpu: others,
                idle_secs: sleuth_lanes::idle_seconds(),
                // What the readers already hold is theirs to keep: without
                // it the limit would fall as readers start, and rise again.
                free_ram_mb: sleuth_lanes::free_ram_mb() + c.group.working_set_mb(),
                on_battery: sleuth_lanes::on_battery(),
                server: self.server_mode,
                speed,
                busy_cpu_percent: s.busy_cpu_percent,
                paused: s.pause_reading,
                ocr_worker_mb: OCR_WORKER_MB,
                fast_backlog: backlog_allowed && fast_wait,
                ocr_backlog: backlog_allowed && ocr_wait,
            },
            Instant::now(),
        );
        // A file someone asked for is read even when reading is paused.
        if urgent {
            t.fast = t.fast.max(1);
            t.ocr = t.ocr.max(1);
        }
        c.workers.set_target(Lane::Fast.name(), t.fast);
        c.workers.set_target(Lane::Ocr.name(), t.ocr);
        self.feed(c, backlog_allowed);
        let events = c.workers.poll(Duration::from_millis(400));
        let idle = events.is_empty() && c.flights.is_empty();
        for (lane, ev) in events {
            let lane = if lane == Lane::Ocr.name() { Lane::Ocr } else { Lane::Fast };
            self.handle(c, lane, ev);
        }
        // Publish: enough waiting, the oldest waiting long, or nothing left to do.
        let total = c.office.unpublished + c.own.unpublished;
        let oldest = [c.office.since, c.own.since].into_iter().flatten().map(|t| t.elapsed()).max().unwrap_or_default();
        if total >= PUBLISH_DOCS || oldest > PUBLISH_AFTER || (total > 0 && idle && oldest > Duration::from_secs(20)) {
            self.publish(c);
        }
        // What the status bar says.
        let (fast_n, fast_busy) = c.workers.counts(Lane::Fast.name());
        let (ocr_n, ocr_busy) = c.workers.counts(Lane::Ocr.name());
        let reading = fast_busy + ocr_busy > 0;
        let current = c.workers.in_flight().into_iter().max_by_key(|(lane, _)| *lane == Lane::Ocr.name()).map(|(_, j)| j.path);
        let lane_trouble = [Lane::Fast, Lane::Ocr].iter().find_map(|l| c.workers.lane_error(l.name()));
        let (state_text, note) = if let Some(why) = lane_trouble {
            ("stopped".to_string(), format!("Readers cannot start: {why}"))
        } else if s.pause_reading && !reading {
            ("paused".to_string(), "Reading is paused on this PC until you resume it.".to_string())
        } else if first_walk && !reading {
            ("waiting".to_string(), format!("Reading starts when the file list of your reading folders is complete ({} files found so far).", fmt_count(self.catalog.file_count())))
        } else if store_away && work_full {
            ("paused: office index away".to_string(), "The office index folder is not reachable; reading carries on when it is back.".to_string())
        } else if reading {
            ("reading".to_string(), t.note.clone())
        } else if t.state.starts_with("paused") {
            (t.state.clone(), t.note.clone())
        } else {
            ("idle".to_string(), String::new())
        };
        {
            let mut st = self.status.lock().unwrap();
            st.fast_workers = fast_n;
            st.ocr_workers = ocr_n;
            st.reader_state = state_text;
            st.reader_note = note;
            st.current_file = current;
            st.store.unpublished = c.office.unpublished + c.own.unpublished;
        }
        if idle && !reading {
            self.reader_signal.wait(Duration::from_secs(if backlog_allowed && (fast_wait || ocr_wait) { 1 } else { 15 }));
        }
        Ok(())
    }

    /// Hand waiting files to idle readers.
    fn feed(&self, c: &mut Coordinator, backlog: bool) {
        let s = self.settings.read().unwrap().clone();
        for lane in [Lane::Fast, Lane::Ocr] {
            loop {
                let (alive, busy) = c.workers.counts(lane.name());
                if busy >= alive {
                    break;
                }
                // Files someone asked for come first.
                let urgent = {
                    let mut u = self.urgent.lock().unwrap();
                    let at = u.iter().position(|j| {
                        let wants_ocr = FileKind::of(&j.path).needs_ocr() || self.pool.state_of(&j.path, j.size, j.mtime) == Some(state::NEEDS_OCR);
                        (lane == Lane::Ocr) == wants_ocr
                    });
                    at.and_then(|i| u.remove(i))
                };
                let job = match urgent {
                    Some(j) => j,
                    None if backlog => {
                        let next = {
                            let mut q = self.queue.lock().unwrap();
                            let list = if lane == Lane::Ocr { &mut q.ocr } else { &mut q.fast };
                            let item = list.pop_front();
                            if list.len() < 50 && item.is_some() {
                                self.stats_signal.raise();
                            }
                            item
                        };
                        match next {
                            Some(i) => Job { path: i.path, size: i.size, mtime: i.mtime, urgent: false },
                            None => break,
                        }
                    }
                    None => break,
                };
                let key = path_key(&job.path);
                if c.flights.contains(&key) {
                    continue;
                }
                let kind = FileKind::of(&job.path);
                if !kind.readable() || (s.kinds_not_read.contains(&kind) && !job.urgent) {
                    continue;
                }
                // Read meanwhile (by this PC or, as far as it knows, another).
                match self.pool.state_of(&job.path, job.size, job.mtime) {
                    Some(state::READ) | Some(state::FAILED) if !job.urgent => continue,
                    Some(state::NEEDS_OCR) if lane == Lane::Fast => continue,
                    _ => {}
                }
                if !job.urgent && self.retry.lock().unwrap().get(&key).is_some_and(|r| r.until > now()) {
                    continue;
                }
                let timeout = if lane == Lane::Ocr { OCR_TIMEOUT } else { FAST_TIMEOUT };
                match c.workers.submit(lane.name(), job, timeout) {
                    Ok(()) => {
                        c.flights.insert(key);
                    }
                    Err(job) => {
                        // No reader is ready for it yet (they are starting).
                        if job.urgent {
                            self.urgent.lock().unwrap().push_front(job);
                        } else {
                            let mut q = self.queue.lock().unwrap();
                            let list = if lane == Lane::Ocr { &mut q.ocr } else { &mut q.fast };
                            list.push_front(Item { path: job.path, size: job.size, mtime: job.mtime });
                        }
                        break;
                    }
                }
            }
        }
    }

    fn defer(&self, key: u64, secs: i64, attempts: u32, reason: &str) {
        self.retry.lock().unwrap().insert(key, Retry { until: now() + secs, attempts, reason: reason.into() });
    }

    /// What a reader answered (or that it died or ran out of time).
    fn handle(&self, c: &mut Coordinator, lane: Lane, ev: Event<Job, Outcome>) {
        let (job, outcome, crashed) = match ev {
            Event::Done { job, reply, .. } => (job, reply, false),
            Event::Crashed { job, detail } => {
                warn!("the reader crashed on {}: {detail}", job.path);
                (job, Outcome::Failed { reason: "the reader crashed on this file".into(), transient: false }, true)
            }
            Event::TimedOut { job, millis } => (job, Outcome::Failed { reason: format!("reading took longer than {} minutes and was stopped", millis / 60_000), transient: false }, false),
        };
        let key = path_key(&job.path);
        c.flights.remove(&key);
        match outcome {
            Outcome::Read { hash, text } | Outcome::Reused { hash, text } => {
                let pages = text.pages.len() as u32;
                if !text.model.is_empty() {
                    let mut st = self.status.lock().unwrap();
                    st.ocr_model = text.model.clone();
                    st.ocr_backend = if text.provenance.backend.is_empty() { "CPU".into() } else { text.provenance.backend.clone() };
                }
                if text.provenance.obtained == "shared cache" {
                    self.cache_hits.fetch_add(1, Ordering::Relaxed);
                } else if lane == Lane::Ocr {
                    self.cache_misses.fetch_add(1, Ordering::Relaxed);
                }
                let deriver = self.deriver();
                match self.save(c, &job, |store, id| store.save_read(id, &hash, &text, now(), &deriver), state::READ) {
                    Ok(()) => {
                        self.retry.lock().unwrap().remove(&key);
                        self.pages_read.lock().unwrap().push_back((Instant::now(), pages));
                    }
                    Err(e) => {
                        warn!("could not save the reading of {}: {e:#}", job.path);
                        self.defer(key, 300, 0, "could not be saved; trying again");
                    }
                }
                self.notify_waiters(&job.path);
            }
            Outcome::NeedsOcr => {
                // Recorded for the whole office: any PC's OCR lane may take
                // it without opening it first.
                if let Err(e) = self.save(c, &job, |store, id| store.save_needs_ocr(id, now()), state::NEEDS_OCR) {
                    warn!("{}: {e:#}", job.path);
                }
                if job.urgent {
                    self.urgent.lock().unwrap().push_back(job);
                } else {
                    self.stats_signal.raise();
                }
            }
            Outcome::HeldBy { host, until } => {
                // Another PC has it: its reading will be in the office index.
                self.claims_waited.fetch_add(1, Ordering::Relaxed);
                self.defer(key, (until - now()).clamp(60, 900), 0, &format!("being read by {host}; its result will be shared"));
            }
            Outcome::Later { reason } => self.defer(key, 120, 0, &reason),
            Outcome::NotHere { reason } => {
                // Another PC may have what this one lacks; ask again later.
                self.defer(key, 6 * 3600, 0, &reason);
                if job.urgent {
                    self.notify_waiters(&job.path);
                }
            }
            Outcome::Failed { reason, transient } => {
                let attempts = self.retry.lock().unwrap().get(&key).map(|r| r.attempts).unwrap_or(0) + 1;
                // A crash may be the PC's (out of memory), so it gets one more go.
                if (transient && attempts < 5) || (crashed && attempts < 2) {
                    self.defer(key, 300 * (1i64 << attempts.min(6)), attempts, &reason);
                    info!("will retry {} later: {reason}", job.path);
                } else {
                    warn!("could not read {}: {reason}", job.path);
                    self.event("warn", &format!("could not read {}: {reason}", job.path));
                    if let Err(e) = self.save(c, &job, |store, id| store.save_failure(id, &reason, now()), state::FAILED) {
                        warn!("{}: {e:#}", job.path);
                    }
                    self.retry.lock().unwrap().remove(&key);
                    self.notify_waiters(&job.path);
                }
            }
        }
    }

    /// Record an outcome for a file in the right `work` database and in the
    /// coverage, so it is searchable and out of the queue at once.
    fn save(&self, c: &mut Coordinator, job: &Job, write: impl FnOnce(&mut Store, i64) -> Result<()>, st: i64) -> Result<()> {
        let _gate = self.work_gate.lock().unwrap();
        let work = if is_local(&job.path) { &mut c.own } else { &mut c.office };
        let (dir, name) = split_path(&job.path);
        let (id, _) = work.store.upsert_file(&job.path, &dir, &name, job.size, job.mtime, 0, state::WAITING)?;
        write(&mut work.store, id)?;
        work.unpublished += 1;
        work.since.get_or_insert_with(Instant::now);
        self.pool.note_saved(&work.part, &job.path, Covered { slot: work.part.slot, id, size: job.size, mtime: job.mtime, state: st, read_at: now() });
        Ok(())
    }

    /// Publish what waits in `work` as parts, then empty it.
    fn publish(&self, c: &mut Coordinator) {
        let Some(store) = self.store_dir() else { return };
        let own_dir = store.join("pcs").join(root_key(&self.pc)).join("text");
        let office_dir = store.join("text");
        let _gate = self.work_gate.lock().unwrap();
        let mut published = false;
        for (work, dir) in [(&mut c.office, &office_dir), (&mut c.own, &own_dir)] {
            if work.unpublished == 0 {
                continue;
            }
            match pool::publish(&work.store, dir, &self.pc) {
                Ok(Some(_)) => {
                    // The part is opened before the copy here is emptied, so
                    // no search ever misses these documents.
                    self.pool.set_dirs(vec![office_dir.clone(), own_dir.clone()]);
                    self.pool.refresh();
                    if let Err(e) = work.store.clear_readings() {
                        warn!("could not empty {}: {e:#}", work.path.display());
                    }
                    work.unpublished = 0;
                    work.since = None;
                    published = true;
                }
                Ok(None) => {
                    work.unpublished = 0;
                    work.since = None;
                }
                Err(e) => {
                    warn!("readings not published yet: {e:#}");
                    // Try again in a while, not on every pass.
                    work.since = Some(Instant::now());
                }
            }
        }
        if published {
            self.pool.recover(&|p| self.presence(p));
            self.stats_signal.raise();
        }
    }

    // ---- moving a 1.0 index into the office index --------------------------

    /// Sleuth 1.0 kept everything in `index.db` on this PC. Its readings
    /// are published to the office index (those the office does not have
    /// yet), checked, and then the file is deleted.
    fn import_old_index(&self, c: &mut Coordinator) {
        let old_path = self.data.join("index.db");
        if !old_path.exists() || self.store_dir().is_none() {
            return;
        }
        let size = std::fs::metadata(&old_path).map(|m| m.len()).unwrap_or(0);
        info!("moving this PC's Sleuth 1.0 index ({} MB) into the office index", size / 1_048_576);
        self.set_reader_state("moving the old index", None);
        let r = (|| -> Result<(u64, u64)> {
            let old = Store::open(&old_path)?;
            let rows: Vec<(i64, String, i64, i64)> = {
                let mut st = old.conn.prepare("SELECT id, path, size, COALESCE(read_at, 0) FROM files WHERE read_state=?1 AND hash IS NOT NULL")?;
                st.query_map([state::READ], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
            };
            let total = rows.len() as u64;
            let deriver = self.deriver();
            let (mut moved, mut done) = (0u64, 0u64);
            for (id, path, _, read_at) in rows {
                if self.stopping() {
                    anyhow::bail!("stopped before the end; it carries on at the next start");
                }
                done += 1;
                let stored = self.stored_path(&path);
                // Already in the office index, as new or newer.
                if self.pool.covered(&stored).is_some_and(|cv| cv.state == state::READ && cv.read_at >= read_at) {
                    continue;
                }
                let work = if is_local(&stored) { &mut c.own } else { &mut c.office };
                let new_id = work.store.import_reading_as(&old, id, Some(&stored), &deriver)?;
                let (size, mtime): (i64, i64) = work.store.conn.query_row("SELECT size, mtime FROM files WHERE id=?1", [new_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                self.pool.note_saved(&work.part, &stored, Covered { slot: work.part.slot, id: new_id, size: size as u64, mtime, state: state::READ, read_at });
                work.unpublished += 1;
                work.since.get_or_insert_with(Instant::now);
                moved += 1;
                if moved.is_multiple_of(PUBLISH_DOCS) {
                    self.set_reader_note(&format!("Moving this PC's earlier readings into the office index: {} of {}.", fmt_count(done), fmt_count(total)));
                    self.publish(c);
                }
            }
            self.publish(c);
            if c.office.unpublished + c.own.unpublished > 0 {
                anyhow::bail!("the last readings could not be published; the old index is kept for now");
            }
            Ok((moved, total))
        })();
        match r {
            Ok((moved, total)) => {
                // Every reading is in a published part now: the old file goes.
                for name in ["index.db", "index.db-wal", "index.db-shm", "index.v1.bak.db"] {
                    let _ = std::fs::remove_file(self.data.join(name));
                }
                let msg = format!("This PC's Sleuth 1.0 index was moved into the office index: {} of its {} read documents were new to the office. The old file ({} MB) was deleted.", fmt_count(moved), fmt_count(total), size / 1_048_576);
                info!("{msg}");
                self.event("info", &msg);
            }
            Err(e) => {
                warn!("the old index was not moved: {e:#}");
                self.event("warn", &format!("This PC's Sleuth 1.0 index was not moved into the office index yet: {e:#}"));
            }
        }
        self.set_reader_note("");
    }

    // ---- merging small parts ---------------------------------------------

    /// Merge small parts of the office index into a larger one, when there
    /// are enough of them and this PC gets the lease for it.
    pub(crate) fn maybe_merge(self: &Arc<Self>) {
        let Some(store) = self.store_dir() else { return };
        let dir = store.join("text");
        if sleuth_lanes::on_battery() && !self.server_mode {
            return;
        }
        let sources = pool::merge_candidates(&self.pool.parts_in(&dir));
        if sources.len() < 2 || MERGING.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(Acquire::Mine(lease)) = self.leases().map(|l: Leases| l.acquire("compact", Duration::from_secs(600))) else {
            MERGING.store(false, Ordering::SeqCst);
            return;
        };
        let e = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("sleuth-merge".into()).spawn(move || {
            crate::sys::enter_background_mode();
            let lease = std::sync::Mutex::new((lease, Instant::now()));
            let keep_going = || {
                let mut g = lease.lock().unwrap();
                if g.1.elapsed() > Duration::from_secs(60) {
                    g.1 = Instant::now();
                    if !g.0.renew() {
                        return false;
                    }
                }
                !e.stopping()
            };
            let deriver = e.deriver();
            // Built on this PC's disk; what an earlier, interrupted merge left there goes first.
            let build = e.data.join("merging");
            if let Ok(rd) = std::fs::read_dir(&build) {
                for f in rd.flatten() {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            match pool::merge(&sources, &|slot, id| !e.pool.is_superseded(slot, id), &dir, &build, &e.pc, &deriver, &keep_going) {
                Ok(Some(_)) => {
                    e.pool.refresh();
                    e.pool.recover(&|p| e.presence(p));
                    e.event("info", &format!("{} small parts of the office index were merged into one", sources.len()));
                }
                Ok(None) => {}
                Err(err) => warn!("merge: {err:#}"),
            }
            if let Ok(g) = lease.into_inner() {
                g.0.release();
            }
            MERGING.store(false, Ordering::SeqCst);
        });
        if spawned.is_err() {
            MERGING.store(false, Ordering::SeqCst);
        }
    }
}

/// Whether a path is one this PC alone can open.
#[allow(dead_code)]
fn local_path(path: &Path) -> bool {
    is_local(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_into_folder_and_name() {
        assert_eq!(split_path(r"\\10.0.0.5\p\Iso\a.pdf"), (r"\\10.0.0.5\p\Iso".to_string(), "a.pdf".to_string()));
        assert_eq!(split_path(r"C:\a.pdf"), (r"C:\".to_string(), "a.pdf".to_string()));
        assert!(is_local(r"D:\x\y.pdf") && !is_local(r"\\srv\s\y.pdf"));
    }
}
