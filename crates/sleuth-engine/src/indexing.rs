//! Stage 1, finding files: keeping the in-memory catalog current.
//!
//! - Every share is listed by **one PC at a time for the whole office**: the
//!   holder of the lease `walk-<share>` in the office store. It publishes
//!   what it lists as segments; every other PC follows them. A PC that
//!   stops mid-walk loses its lease after ten minutes and another carries on
//!   from the same pass.
//! - A PC's own disks are listed by that PC alone and their catalog stays
//!   on it (a few megabytes).
//! - The office server lists only its projects' folders: it publishes
//!   segments for them without claiming to have listed the whole share.
//!
//! The follower thread loads catalogs, applies what other PCs published,
//! keeps the parts pool in step with the shared folder, writes this PC's
//! heartbeat and starts merges of small parts.

use crate::crawl::{Entry, WalkConfig, WalkEvent, list_with_timeout, norm_dir, walk_resumable};
use crate::pool::{self, Presence};
use crate::service::{Engine, now, root_display};
use crate::usn;
use anyhow::Result;
use sleuth_core::paths::{DriveKind, root_key, root_of};
use sleuth_core::protocol::PcStatus;
use sleuth_office::{Acquire, Identity, Lease, Leases, Listing, Mark, RootFiles, RootState, SegmentWriter};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// A walk lease is renewed every minute and lost after ten without renewal.
const WALK_LEASE: Duration = Duration::from_secs(600);
/// Listings are published at least this often during a walk.
const SEGMENT_EVERY: Duration = Duration::from_secs(60);
const SEGMENT_LISTINGS: usize = 3000;

/// The folder a path is in, in the catalog's form (`C:\` for a drive root).
pub(crate) fn parent_dir(path: &str) -> Option<String> {
    let (dir, _) = path.rsplit_once('\\')?;
    Some(if dir.len() == 2 && dir.ends_with(':') { format!("{dir}\\") } else { dir.to_string() })
}

/// Something to list: a whole root, or (office server) one folder of it.
#[derive(Debug, Clone)]
struct Target {
    /// Where the walk starts.
    folder: String,
    /// The catalog root it belongs to.
    root: String,
    whole: bool,
}

/// What the walk keeps per target while it runs.
struct Walking {
    target: Target,
    pass: i64,
    writer: SegmentWriter,
    files: Arc<RootFiles>,
    lease: Option<Lease>,
    local: bool,
    folders: u64,
    files_seen: u64,
    skipped: u64,
    note: String,
    started: i64,
    done: bool,
    offline: bool,
}

impl Engine {
    pub(crate) fn leases(&self) -> Option<Leases> {
        let dir = self.store_dir()?.join("leases");
        Some(Leases::new(dir, Identity { host: self.pc.clone(), user: std::env::var("USERNAME").unwrap_or_default(), pid: std::process::id() }))
    }

    fn is_local_root(&self, root: &str) -> bool {
        !root.starts_with("\\\\")
    }

    fn catalog_dir(&self, root: &str) -> Option<PathBuf> {
        if self.is_local_root(root) { Some(self.data.join("catalog").join(root_key(root))) } else { self.store_dir().map(|d| d.join("catalog").join(root_key(root))) }
    }

    /// What this PC lists: every drive whole, or the server's folders.
    fn targets(&self) -> Vec<Target> {
        self.walk_roots()
            .into_iter()
            .map(|folder| {
                // Development setups list a test folder as if it were a
                // whole share (`SLEUTH_FOLDER_ROOTS`), leases and all.
                let root = if std::env::var_os("SLEUTH_FOLDER_ROOTS").is_some() { norm_dir(&folder) } else { norm_dir(&root_of(&folder)) };
                let whole = folder.trim_end_matches('\\').eq_ignore_ascii_case(root.trim_end_matches('\\'));
                Target { folder, root, whole }
            })
            .collect()
    }

    /// The catalog root a path is under (lower case, as the roots are keyed).
    pub(crate) fn root_for(&self, path: &str) -> Option<String> {
        let lower = path.to_lowercase();
        self.roots
            .lock()
            .unwrap()
            .keys()
            .filter(|k| {
                let r = k.trim_end_matches('\\');
                lower == r || lower.starts_with(&format!("{r}\\"))
            })
            .max_by_key(|k| k.len())
            .cloned()
    }

    /// Whether a file a reading belongs to is still on disk, as far as the
    /// catalog knows.
    pub(crate) fn presence(&self, path: &str) -> Presence {
        if self.catalog.get(path).is_some() {
            return Presence::Here;
        }
        match parent_dir(path) {
            Some(dir) if self.catalog.dir_listed(&dir) => Presence::Gone,
            _ => Presence::Unknown,
        }
    }

    // ---- following ---------------------------------------------------------

    pub(crate) fn follow_loop(self: Arc<Self>) {
        crate::sys::enter_background_mode();
        let mut last_catalog = 0u64;
        let mut last_sweep: Option<Instant> = None;
        while !self.stopping() {
            *self.drives.write().unwrap() = self.current_drives();
            self.sync_roots();
            // The parts pool: the office folder, and this PC's own for
            // readings of files on its local disks.
            if let Some(store) = self.store_dir() {
                let own = store.join("pcs").join(root_key(&self.pc)).join("text");
                self.pool.set_dirs(vec![store.join("text"), own]);
            } else {
                self.pool.set_dirs(Vec::new());
            }
            let changed = self.pool.refresh();
            let catalog_now = self.catalog.generation();
            if changed || catalog_now != last_catalog {
                last_catalog = catalog_now;
                self.pool.recover(&|p| self.presence(p));
                self.stats_signal.raise();
                self.reader_signal.raise();
            }
            self.heartbeat();
            if last_sweep.is_none_or(|t| t.elapsed() > Duration::from_secs(3600))
                && let Some(store) = self.store_dir()
            {
                last_sweep = Some(Instant::now());
                pool::sweep(&store.join("text"));
                pool::sweep(&store.join("pcs").join(root_key(&self.pc)).join("text"));
                if let Some(l) = self.leases() {
                    l.sweep();
                }
            }
            self.maybe_merge();
            self.follow_signal.wait(Duration::from_secs(60));
        }
    }

    /// Load the catalog of every root this PC sees, drop roots it no longer
    /// sees, and apply what other PCs published since last time.
    fn sync_roots(&self) {
        let targets = self.targets();
        let mut wanted: Vec<String> = Vec::new();
        for t in &targets {
            if !wanted.iter().any(|w| w.eq_ignore_ascii_case(&t.root)) {
                wanted.push(t.root.clone());
            }
        }
        // Forget roots that are no longer mapped.
        {
            let mut roots = self.roots.lock().unwrap();
            let gone: Vec<String> = roots.keys().filter(|k| !wanted.iter().any(|w| w.to_lowercase() == **k)).cloned().collect();
            for k in gone {
                if let Some(r) = roots.remove(&k) {
                    self.catalog.remove_root(&r.root);
                    info!("{} is no longer mapped here; its files are out of this PC's search", r.root);
                }
            }
        }
        // Load new roots, several at once: a big share takes a while.
        let mut to_load: Vec<(String, Arc<RootFiles>, bool)> = Vec::new();
        for root in &wanted {
            let key = root.to_lowercase();
            if self.roots.lock().unwrap().get(&key).is_some_and(|r| r.loaded) {
                continue;
            }
            let Some(dir) = self.catalog_dir(root) else { continue };
            to_load.push((root.clone(), Arc::new(RootFiles::new(dir, root)), self.is_local_root(root)));
        }
        if !to_load.is_empty() {
            self.status.lock().unwrap().index.phase = "loading".into();
            std::thread::scope(|sc| {
                for (root, files, local) in &to_load {
                    sc.spawn(move || {
                        let t = Instant::now();
                        self.catalog.add_root(root);
                        match files.load(&self.catalog) {
                            Ok(mark) => {
                                let (dirs, n) = self.catalog.root_counts(root);
                                info!("{root}: {n} files in {dirs} folders loaded in {} ms", t.elapsed().as_millis());
                                self.roots.lock().unwrap().insert(
                                    root.to_lowercase(),
                                    crate::service::RootInfo { root: root.clone(), local: *local, files: Arc::clone(files), mark, loaded: true, state: files.state(), walking: false },
                                );
                            }
                            Err(e) => warn!("{root}: file list not loaded: {e:#}"),
                        }
                    });
                }
            });
            self.indexer_signal.raise();
        }
        // Follow what other PCs published.
        let following: Vec<(String, Arc<RootFiles>, Mark)> = self.roots.lock().unwrap().iter().filter(|(_, r)| r.loaded && !r.walking).map(|(k, r)| (k.clone(), Arc::clone(&r.files), r.mark)).collect();
        for (key, files, mut mark) in following {
            let applied = match files.follow(&self.catalog, &mut mark) {
                Ok(n) => n,
                Err(e) => {
                    warn!("{key}: could not follow the file list: {e:#}");
                    0
                }
            };
            let state = files.state();
            if let Some(r) = self.roots.lock().unwrap().get_mut(&key)
                && !r.walking
            {
                r.mark = mark;
                r.state = state;
            }
            if applied > 0 {
                self.reader_signal.raise();
            }
        }
        let mut st = self.status.lock().unwrap();
        if st.index.phase == "loading" || st.index.phase.is_empty() || st.index.phase == "starting" {
            st.index.phase = "up to date".into();
        }
    }

    /// Tell the office what this PC is doing, and learn what the others do.
    fn heartbeat(&self) {
        let Some(store) = self.store_dir() else { return };
        let dir = store.join("pcs");
        let (st, _) = match self.status() {
            Ok(s) => s,
            Err(_) => return,
        };
        let walking: Vec<String> = self.roots.lock().unwrap().values().filter(|r| r.walking).map(|r| r.root.clone()).collect();
        let abilities = {
            let a = sleuth_extract::abilities(&sleuth_extract::Tools::find());
            let mut v = Vec::new();
            if a.office_filter {
                v.push("office filter".to_string());
            }
            if a.dgn_filter {
                v.push("dgn filter".to_string());
            }
            if a.dwg {
                v.push("dwg".to_string());
            }
            v
        };
        let me = PcStatus {
            name: self.pc.clone(),
            user: std::env::var("USERNAME").unwrap_or_default(),
            version: env!("CARGO_PKG_VERSION").into(),
            state: if !walking.is_empty() && st.reader_state != "reading" { "listing files".into() } else { st.reader_state.clone() },
            walking,
            docs_per_hour: st.docs_per_hour,
            pages_per_hour: st.pages_per_hour,
            fast_workers: st.fast_workers,
            ocr_workers: st.ocr_workers,
            abilities,
            last_seen: now(),
            server: self.server_mode,
        };
        let write = || -> Result<()> {
            std::fs::create_dir_all(&dir)?;
            let dest = dir.join(format!("{}.json", root_key(&self.pc)));
            let tmp = dir.join(format!("{}.json.tmp", root_key(&self.pc)));
            std::fs::write(&tmp, serde_json::to_vec(&me)?)?;
            std::fs::rename(&tmp, &dest)?;
            Ok(())
        };
        if let Err(e) = write() {
            warn!("heartbeat not written: {e:#}");
        }
        let mut all = vec![me];
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let Some(pc) = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<PcStatus>(&b).ok()) else { continue };
                if pc.name.eq_ignore_ascii_case(&self.pc) || now() - pc.last_seen > 24 * 3600 {
                    continue;
                }
                all.push(pc);
            }
        }
        all[1..].sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(a.name.cmp(&b.name)));
        *self.pcs.lock().unwrap() = all;
    }

    /// PCs reading right now (heartbeat in the last three minutes), by
    /// name, this one included: how the backlog is shared out.
    pub(crate) fn readers_now(&self) -> Vec<String> {
        let mut names: Vec<String> = self.pcs.lock().unwrap().iter().filter(|p| now() - p.last_seen < 180 && p.state != "paused" && !p.state.starts_with("paused")).map(|p| p.name.to_lowercase()).collect();
        if !names.contains(&self.pc.to_lowercase()) {
            names.push(self.pc.to_lowercase());
        }
        names.sort();
        names.dedup();
        names
    }

    // ---- walking -----------------------------------------------------------

    pub(crate) fn indexer_loop(self: Arc<Self>) {
        crate::sys::enter_background_mode();
        while !self.stopping() {
            let paused = self.settings.read().unwrap().pause_walk;
            if paused {
                self.status.lock().unwrap().index.phase = "paused".into();
            } else {
                // A bug in one pass must not stop indexing for the session.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.index_pass()));
                match r {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!("file list pass failed: {e:#}"),
                    Err(_) => warn!("file list pass crashed; retrying next pass"),
                }
            }
            self.indexer_signal.wait(Duration::from_secs(60));
        }
    }

    /// One pass: local disks by their change journal where it can be read,
    /// then a walk of every root that is due and that this PC gets the
    /// lease for.
    fn index_pass(&self) -> Result<()> {
        let s = self.settings.read().unwrap().clone();
        let interval = s.name_rescan_minutes.max(15) as i64 * 60;
        let force_all = self.full_walk.swap(false, Ordering::SeqCst);
        let force_one = self.walk_root.lock().unwrap().take().and_then(|r| self.root_for(&r));
        let drives = self.drives.read().unwrap().clone();
        let leases = self.leases();
        let mut journal_used = Vec::new();
        let mut journal_off = Vec::new();
        let mut restart: Vec<(String, String, i64)> = Vec::new();
        let mut due: Vec<(Target, Option<Lease>)> = Vec::new();
        for t in self.targets() {
            let key = t.root.to_lowercase();
            let (loaded, state, local) = match self.roots.lock().unwrap().get(&key) {
                Some(r) => (r.loaded, r.state.clone().unwrap_or_default(), r.local),
                None => continue, // not loaded yet (the shared folder is away)
            };
            if !loaded {
                continue;
            }
            let forced = force_all || force_one.as_deref() == Some(key.as_str());
            if t.whole && local && !forced && drives.drives.iter().any(|d| d.kind == DriveKind::Fixed && norm_dir(&d.root()).eq_ignore_ascii_case(&t.root)) {
                // A local NTFS disk: its change journal says what changed.
                let letter = t.root.trim_end_matches('\\').to_string();
                let checkpoint = self.with_local(|l| l.checkpoint(&letter)).ok().flatten();
                match usn::changes(&letter, checkpoint) {
                    usn::Outcome::Unavailable(why) => journal_off.push(format!("{letter} {why}")),
                    usn::Outcome::Restart { journal_id, next_usn, why } => {
                        self.event("info", &format!("{letter}: full listing ({why})"));
                        restart.push((t.root.to_lowercase(), journal_id, next_usn));
                        due.push((t, None));
                        continue;
                    }
                    usn::Outcome::Changes { shallow, deep, journal_id, next_usn } => {
                        journal_used.push(letter.clone());
                        let n = shallow.len() + deep.len();
                        self.apply_journal(&t, &shallow, &deep, &s)?;
                        let _ = self.with_local(|l| l.set_checkpoint(&letter, &journal_id, next_usn, now()));
                        if n > 0 {
                            info!("{letter}: {n} changed folder(s) from the change journal");
                        }
                        continue;
                    }
                }
            }
            let last = if t.whole { state.last_ok } else { self.with_local(|l| l.get_meta(&format!("walked:{}", t.folder.to_lowercase()))).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0) };
            if !forced && last > 0 && now() - last < interval {
                continue;
            }
            if !t.whole || local {
                due.push((t, None));
                continue;
            }
            // A share: one PC lists it for the office.
            let Some(leases) = &leases else { continue };
            match leases.acquire(&format!("walk-{}", root_key(&t.root)), WALK_LEASE) {
                Acquire::Mine(l) => due.push((t, Some(l))),
                Acquire::HeldBy(h) => info!("{} is being listed by {}", t.root, h.host),
                Acquire::Unavailable(why) => warn!("{}: no lease ({why}); not listed now", t.root),
            }
        }
        *self.journal_note.lock().unwrap() = if !journal_used.is_empty() {
            format!("in use on {}", journal_used.join(", "))
        } else if !journal_off.is_empty() {
            format!("not used: {}; local disks are walked instead", journal_off.join("; "))
        } else {
            "no local NTFS disks indexed".into()
        };
        if due.is_empty() {
            let mut st = self.status.lock().unwrap();
            if st.index.phase != "loading" {
                st.index.phase = "up to date".into();
            }
            return Ok(());
        }
        info!("file list: listing {} root(s)", due.len());
        let completed = self.walk(due, &s);
        for (root, id, usn) in restart {
            if completed.iter().any(|r| r.to_lowercase() == root) {
                let letter = root.trim_end_matches('\\').to_uppercase();
                let _ = self.with_local(|l| l.set_checkpoint(&letter, &id, usn, now()));
            }
        }
        Ok(())
    }

    fn listing(dir: &str, entries: &[Entry], pass: i64) -> Listing {
        Listing {
            dir: dir.to_string(),
            pass,
            listed_at: now(),
            subdirs: entries.iter().filter(|e| e.is_dir).map(|e| e.name.clone()).collect(),
            files: entries.iter().filter(|e| !e.is_dir).map(|e| (e.name.clone(), e.size, e.mtime)).collect(),
        }
    }

    /// List changed folders again; walk folders created or moved in.
    fn apply_journal(&self, t: &Target, shallow: &[String], deep: &[String], s: &sleuth_core::settings::Settings) -> Result<()> {
        let Some((files, at)) = self.roots.lock().unwrap().get(&t.root.to_lowercase()).map(|r| (Arc::clone(&r.files), r.mark.pass)) else { return Ok(()) };
        let excluded: Vec<String> = s.excluded.iter().map(|x| x.to_lowercase()).collect();
        // One-off listings are published within the root's current pass, so
        // every follower applies them after what it already has.
        let mut writer = files.writer(&self.pc, at)?;
        for dir in shallow {
            let d = norm_dir(dir);
            if excluded.iter().any(|x| d.to_lowercase().starts_with(x.as_str())) {
                continue;
            }
            match list_with_timeout(&d, Duration::from_secs(10)) {
                Ok(entries) => {
                    let l = Self::listing(&d, &entries, 0);
                    self.catalog.apply(&l);
                    writer.add(&l);
                }
                // Gone since: its parent's listing (also journaled) reconciles it.
                Err(e) => info!("journal: {d} not listed: {e}"),
            }
        }
        writer.flush()?;
        if !deep.is_empty() {
            let targets = deep.iter().map(|d| (Target { folder: norm_dir(d), root: t.root.clone(), whole: false }, None)).collect();
            self.walk(targets, s);
        }
        if !shallow.is_empty() || !deep.is_empty() {
            self.stats_signal.raise();
            self.reader_signal.raise();
        }
        Ok(())
    }

    /// Walk the targets (resuming an interrupted pass of a whole root).
    /// Returns the roots listed to the end.
    fn walk(&self, due: Vec<(Target, Option<Lease>)>, s: &sleuth_core::settings::Settings) -> Vec<String> {
        self.walk_cancel.store(false, Ordering::SeqCst);
        let mut walks: Vec<Walking> = Vec::new();
        for (target, lease) in due {
            let key = target.root.to_lowercase();
            let Some((files, state, local, at)) = self.roots.lock().unwrap().get(&key).map(|r| (Arc::clone(&r.files), r.state.clone().unwrap_or_default(), r.local, r.mark.pass)) else { continue };
            // Carry on a pass another PC (or an earlier run) left unfinished
            // less than a day ago; otherwise start a new one.
            let pass = if !target.whole {
                0
            } else if matches!(state.phase.as_str(), "walking" | "partial") && state.pass > 0 && now() - state.pass < 86_400 && state.last_ok < state.pass {
                info!("{}: carrying on the listing started {} s ago", target.root, now() - state.pass);
                state.pass
            } else {
                now()
            };
            // A folder listed by itself publishes within the root's current pass.
            let writer = match files.writer(&self.pc, if target.whole { pass } else { at }) {
                Ok(w) => w,
                Err(e) => {
                    warn!("{}: cannot write its file list: {e:#}", target.root);
                    continue;
                }
            };
            if target.whole
                && let Some(r) = self.roots.lock().unwrap().get_mut(&key)
            {
                r.walking = true;
            }
            walks.push(Walking { target, pass, writer, files, lease, local, folders: 0, files_seen: 0, skipped: 0, note: String::new(), started: now(), done: false, offline: false });
        }
        if walks.is_empty() {
            return Vec::new();
        }
        {
            let mut st = self.status.lock().unwrap();
            st.index.phase = "walking".into();
            st.index.pass_started = now();
            st.index.folders_listed = 0;
            st.index.folders_pending = walks.len() as u64;
            st.index.files_seen = 0;
        }
        let scopes = self.read_scopes();
        let cfg = WalkConfig {
            per_server: s.listings_per_server.clamp(1, 16) as usize,
            local_threads: 4,
            listing_timeout: Duration::from_secs(s.listing_timeout_secs.max(5) as u64),
            excluded: s.excluded.iter().map(|x| x.to_lowercase()).collect(),
            background: true,
            first: scopes.iter().map(|x| x.to_lowercase()).collect(),
        };
        let folders: Vec<String> = walks.iter().map(|w| w.target.folder.clone()).collect();
        let passes: Vec<(String, i64)> = walks.iter().filter(|w| w.target.whole).map(|w| (w.target.root.trim_end_matches('\\').to_lowercase(), w.pass)).collect();
        // A folder listed in this pass is not listed again (its known
        // subfolders are walked instead).
        let done = |d: &str| -> Option<Vec<String>> {
            let lower = d.to_lowercase();
            let (_, pass) = passes.iter().find(|(r, _)| lower == *r || lower.starts_with(&format!("{r}\\")))?;
            self.catalog.listed_in_pass(d, *pass)
        };
        let walks = Mutex::new(walks);
        let (tx, rx) = std::sync::mpsc::sync_channel::<WalkEvent>(256);
        std::thread::scope(|sc| {
            sc.spawn(|| {
                let mut last_flush = Instant::now();
                let mut last_note = Instant::now();
                for ev in rx {
                    let mut ws = walks.lock().unwrap();
                    let find = |ws: &mut Vec<Walking>, root: &str| ws.iter().position(|w| w.target.folder.eq_ignore_ascii_case(root));
                    match ev {
                        WalkEvent::Listed { root, dir, entries } => {
                            let Some(i) = find(&mut ws, &root) else { continue };
                            let w = &mut ws[i];
                            let l = Self::listing(&dir, &entries, w.pass);
                            let change = self.catalog.apply(&l);
                            w.writer.add(&l);
                            w.folders += 1;
                            w.files_seen += l.files.len() as u64;
                            let mut st = self.status.lock().unwrap();
                            st.index.folders_listed += 1;
                            st.index.folders_pending = (st.index.folders_pending + l.subdirs.len() as u64).saturating_sub(1);
                            st.index.files_seen += l.files.len() as u64;
                            drop(st);
                            if change.files_added + change.files_changed > 0 && last_note.elapsed() > Duration::from_secs(5) {
                                last_note = Instant::now();
                                self.stats_signal.raise();
                                self.reader_signal.raise();
                            }
                        }
                        WalkEvent::Skipped { root, dir, reason } => {
                            info!("skipped {dir}: {reason}");
                            let mut st = self.status.lock().unwrap();
                            st.index.folders_pending = st.index.folders_pending.saturating_sub(1);
                            drop(st);
                            if let Some(i) = find(&mut ws, &root) {
                                ws[i].skipped += 1;
                                ws[i].note = format!("{} skipped, e.g. {dir}: {reason}", ws[i].skipped);
                            }
                        }
                        WalkEvent::RootOffline { root, reason } => {
                            info!("offline {root}: {reason}");
                            self.event("warn", &format!("{root} offline: {reason}"));
                            let mut st = self.status.lock().unwrap();
                            st.index.folders_pending = st.index.folders_pending.saturating_sub(1);
                            drop(st);
                            if let Some(i) = find(&mut ws, &root) {
                                ws[i].offline = true;
                                ws[i].note = reason;
                                self.finish(&mut ws[i]);
                            }
                        }
                        WalkEvent::RootDone { root } => {
                            if let Some(i) = find(&mut ws, &root) {
                                ws[i].done = true;
                                self.finish(&mut ws[i]);
                            }
                        }
                    }
                    // Publish what was listed, renew the leases, and say
                    // where the walk is.
                    if last_flush.elapsed() > SEGMENT_EVERY || ws.iter().any(|w| w.writer.pending() >= SEGMENT_LISTINGS) {
                        last_flush = Instant::now();
                        for w in ws.iter_mut().filter(|w| !w.done && !w.offline) {
                            if let Err(e) = w.writer.flush() {
                                warn!("{}: file list not published: {e:#}", w.target.root);
                            }
                            if let Some(l) = &mut w.lease
                                && !l.renew()
                            {
                                warn!("{}: the listing lease was lost; another PC carries on", w.target.root);
                                self.walk_cancel.store(true, Ordering::SeqCst);
                            }
                            self.write_state(w, "walking");
                        }
                    }
                }
            });
            walk_resumable(&folders, &cfg, &self.walk_cancel, &done, &|ev| {
                let _ = tx.send(ev);
            });
            drop(tx);
        });
        let mut ws = walks.into_inner().unwrap();
        let mut completed = Vec::new();
        for w in ws.iter_mut() {
            if w.done {
                completed.push(w.target.root.clone());
            } else if !w.offline {
                // Stopped before the end (pause, shutdown, lost lease): what
                // was listed is published; the pass carries on later.
                w.note = if w.note.is_empty() { "listing stopped before the end".into() } else { w.note.clone() };
                self.finish(w);
            }
        }
        {
            let mut st = self.status.lock().unwrap();
            st.index.phase = if self.settings.read().unwrap().pause_walk { "paused".into() } else { "up to date".into() };
            st.index.pass_started = 0;
            st.index.folders_pending = 0;
        }
        info!("file list: {} of {} root(s) listed to the end", completed.len(), ws.len());
        self.stats_signal.raise();
        self.reader_signal.raise();
        completed
    }

    fn write_state(&self, w: &Walking, phase: &str) {
        if !w.target.whole {
            return;
        }
        let key = w.target.root.to_lowercase();
        let last_ok = self.roots.lock().unwrap().get(&key).and_then(|r| r.state.as_ref().map(|s| s.last_ok)).unwrap_or(0);
        let (dirs, files) = self.catalog.root_counts(&w.target.root);
        let state = RootState {
            root: w.target.root.clone(),
            pass: w.pass,
            phase: phase.into(),
            by: self.pc.clone(),
            started: w.started,
            updated: now(),
            last_ok: if phase == "done" { now() } else { last_ok },
            folders: dirs,
            files,
            skipped: w.skipped,
            note: w.note.clone(),
        };
        if let Err(e) = w.files.set_state(&state) {
            warn!("{}: state not written: {e:#}", w.target.root);
        }
        if let Some(r) = self.roots.lock().unwrap().get_mut(&key) {
            r.state = Some(state);
        }
    }

    /// A target is over (done, offline, or stopped): publish, record, let go.
    fn finish(&self, w: &mut Walking) {
        let mark = match w.writer.flush() {
            Ok(m) => m,
            Err(e) => {
                warn!("{}: file list not published: {e:#}", w.target.root);
                None
            }
        };
        let key = w.target.root.to_lowercase();
        if !w.target.whole {
            if w.done {
                let _ = self.with_local(|l| l.set_meta(&format!("walked:{}", w.target.folder.to_lowercase()), &now().to_string()));
            }
            return;
        }
        let phase = if w.offline {
            "offline"
        } else if !w.done {
            "partial"
        } else if w.skipped > 0 {
            // Listed to the end, but some folders did not answer.
            "done"
        } else {
            "done"
        };
        if w.done {
            // The whole root as one file, so a PC starting later loads it
            // in one read instead of every segment.
            let at = mark.or_else(|| self.roots.lock().unwrap().get(&key).map(|r| r.mark)).unwrap_or(Mark { pass: w.pass, seq: 0 });
            let at = Mark { pass: w.pass.max(at.pass), seq: at.seq };
            if let Err(e) = w.files.write_full(&self.catalog, at) {
                warn!("{}: full file list not written: {e:#}", w.target.root);
            }
            let drives = self.drives.read().unwrap().clone();
            let (dirs, files) = self.catalog.root_counts(&w.target.root);
            info!("{}: listed to the end ({files} files in {dirs} folders{})", root_display(&w.target.root, &drives), if w.skipped > 0 { format!(", {} folders skipped", w.skipped) } else { String::new() });
        }
        self.write_state(w, phase);
        if let Some(r) = self.roots.lock().unwrap().get_mut(&key) {
            r.walking = false;
            if let Some(m) = mark {
                r.mark = m;
            }
        }
        if let Some(l) = w.lease.take() {
            l.release();
        }
        let _ = w.local;
    }

    /// True when every root holding a reading folder has been listed to the
    /// end at least once (by any PC), so everything in them can be queued.
    pub(crate) fn reading_roots_listed(&self) -> bool {
        let scopes = self.read_scopes();
        // Folders listed by themselves (the office server's projects) are
        // done when their own walk finished, whatever the rest of the share.
        let alone: Vec<(String, bool)> = self
            .targets()
            .into_iter()
            .filter(|t| !t.whole)
            .map(|t| {
                let key = t.folder.to_lowercase();
                let done = self.with_local(|l| l.get_meta(&format!("walked:{key}"))).ok().flatten().is_some();
                (key, done)
            })
            .collect();
        let roots = self.roots.lock().unwrap();
        scopes.iter().all(|sc| {
            let lower = sc.to_lowercase();
            if let Some((_, done)) = alone.iter().find(|(f, _)| lower == *f || lower.starts_with(&format!("{}\\", f.trim_end_matches('\\')))) {
                return *done;
            }
            let holder = roots.iter().filter(|(k, _)| {
                let r = k.trim_end_matches('\\');
                lower == r || lower.starts_with(&format!("{r}\\"))
            });
            match holder.max_by_key(|(k, _)| k.len()) {
                Some((_, r)) => r.state.as_ref().is_some_and(|s| s.last_ok > 0),
                // Not in the catalog (not mapped): nothing to wait for.
                None => true,
            }
        })
    }
}
