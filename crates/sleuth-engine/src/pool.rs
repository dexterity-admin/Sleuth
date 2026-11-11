//! The parts pool: every place read documents are searched from.
//!
//! A *part* is a write-once SQLite file in the office store's `text` folder
//! (see `docs/design/office-store.md`), opened read-only and immutable. This
//! PC's own readings wait in `work.db` until they are published as a new
//! part; the pool searches both the same way.
//!
//! - The same document can be in several parts (read again after it changed,
//!   or corrected by someone). The newest reading wins; the others are
//!   hidden from every query, as are readings of files that are gone.
//! - *Coverage* is what the parts hold by path, size and date, so the
//!   reading queue knows a file is done without opening it.
//! - Small parts are merged into larger ones by one PC at a time; the parts
//!   merged away get a `.retired` marker and are deleted a day later, when
//!   no PC still has them open.

use crate::derive::Deriver;
use crate::store::{ReadingRow, Store, state};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tracing::{info, warn};

/// A retired part is deleted this long after it was merged away.
const RETIRED_KEEP_SECS: u64 = 24 * 3600;
/// Parts above this size are never merged again.
const BIGGEST_MERGED: u64 = 2 * 1024 * 1024 * 1024;

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Key of a path in the coverage map (paths compare without case).
pub fn path_key(path: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for c in path.chars().flat_map(char::to_lowercase) {
        c.hash(&mut h);
    }
    h.finish()
}

/// [`path_key`] for many files of the same folders: the folder's part of
/// the key is worked out once per folder, not once per file.
#[derive(Default)]
pub struct PathKeyer {
    dir: String,
    state: Option<std::collections::hash_map::DefaultHasher>,
}

impl PathKeyer {
    pub fn key(&mut self, dir: &str, name: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        if self.state.is_none() || self.dir != dir {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for c in dir.chars().flat_map(char::to_lowercase) {
                c.hash(&mut h);
            }
            if !dir.ends_with('\\') {
                '\\'.hash(&mut h);
            }
            self.dir = dir.to_string();
            self.state = Some(h);
        }
        let mut h = self.state.clone().unwrap_or_default();
        for c in name.chars().flat_map(char::to_lowercase) {
            c.hash(&mut h);
        }
        h.finish()
    }
}

/// Whether a file a reading belongs to is still where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Here,
    /// Its folder was listed and no longer holds it.
    Gone,
    /// Its drive is not in this PC's catalog (not mapped, or not listed yet).
    Unknown,
}

/// The current reading of one path.
#[derive(Debug, Clone, Copy)]
pub struct Covered {
    pub slot: u32,
    /// Row id inside that part.
    pub id: i64,
    pub size: u64,
    pub mtime: i64,
    /// [`state::READ`] or [`state::FAILED`].
    pub state: i64,
    pub read_at: i64,
}

/// A document id that names its part too, for the ids that travel to the
/// window and the office server's clients and come back.
pub fn compose(slot: u32, id: i64) -> i64 {
    ((slot as i64) << 40) | (id & 0xFF_FFFF_FFFF)
}

pub fn split(doc_id: i64) -> (u32, i64) {
    ((doc_id >> 40) as u32, doc_id & 0xFF_FFFF_FFFF)
}

pub struct Part {
    /// Stable for as long as this worker runs; never reused.
    pub slot: u32,
    /// File name without extension; `work` parts are named by their file.
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// This PC's unpublished readings (a live database, not a published part).
    pub work: bool,
    store: Mutex<Store>,
    /// What a published part covers (it never changes).
    rows: Vec<ReadingRow>,
    /// Set when a query failed on I/O: the part is reopened at the next refresh.
    broken: AtomicBool,
}

impl Part {
    /// Run a query on this part. An I/O failure (share gone) marks the part
    /// for reopening and is returned.
    pub fn with<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let g = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let r = f(&g);
        g.conn.flush_prepared_statement_cache();
        if let Err(e) = &r
            && is_io(e)
        {
            self.broken.store(true, Ordering::SeqCst);
        }
        r
    }

    /// As [`Part::with`], for the few operations that need `&mut Store`.
    pub fn with_mut<T>(&self, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        let mut g = self.store.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut g)
    }
}

fn is_io(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(f, _)) if matches!(f.code, rusqlite::ErrorCode::SystemIoFailure | rusqlite::ErrorCode::CannotOpen | rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
        ) || c.downcast_ref::<std::io::Error>().is_some()
    })
}

#[derive(Serialize, Deserialize)]
struct Retired {
    /// The part that now holds these readings.
    into: String,
    at: i64,
}

#[derive(Default)]
pub struct Pool {
    /// Folders of published parts: the office `text` folder, and this PC's
    /// own folder for readings of files on its local drives.
    dirs: RwLock<Vec<PathBuf>>,
    parts: RwLock<Vec<Arc<Part>>>,
    cover: RwLock<HashMap<u64, Covered>>,
    /// Readings a newer reading of the same path replaced, per part.
    superseded: RwLock<HashMap<u32, HashSet<i64>>>,
    next_slot: AtomicU32,
    /// Bumped whenever what a search can see changed.
    generation: AtomicU64,
    /// Parts refused (made by a newer Sleuth, damaged), with the reason, so
    /// each is reported once.
    refused: Mutex<HashMap<PathBuf, String>>,
}

#[derive(Debug, Clone, Default)]
pub struct PoolStats {
    pub parts: u32,
    pub bytes: u64,
    pub documents: u64,
    pub failed: u64,
}

impl Pool {
    pub fn new() -> Pool {
        Pool { next_slot: AtomicU32::new(1), ..Default::default() }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// The folders published parts are looked for in.
    pub fn set_dirs(&self, dirs: Vec<PathBuf>) {
        *self.dirs.write().unwrap() = dirs;
    }

    pub fn parts(&self) -> Vec<Arc<Part>> {
        self.parts.read().unwrap().clone()
    }

    pub fn part(&self, slot: u32) -> Option<Arc<Part>> {
        self.parts.read().unwrap().iter().find(|p| p.slot == slot).cloned()
    }

    /// Add this PC's unpublished readings (`work.db`) to what is searched.
    pub fn add_work(&self, path: &Path) -> Result<Arc<Part>> {
        if let Some(p) = self.parts.read().unwrap().iter().find(|p| p.work && p.path == path) {
            return Ok(Arc::clone(p));
        }
        let store = Store::open(path)?;
        store.conn.execute_batch("PRAGMA query_only=ON;")?;
        let part = Arc::new(Part {
            slot: self.next_slot.fetch_add(1, Ordering::SeqCst),
            name: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            path: path.to_path_buf(),
            bytes: 0,
            work: true,
            store: Mutex::new(store),
            rows: Vec::new(),
            broken: AtomicBool::new(false),
        });
        self.parts.write().unwrap().push(Arc::clone(&part));
        Ok(part)
    }

    /// Search a store this process holds open itself (an import in
    /// progress, and tests). It is treated as unpublished readings.
    pub fn add_store(&self, store: Store, name: &str) -> Arc<Part> {
        let part = Arc::new(Part {
            slot: self.next_slot.fetch_add(1, Ordering::SeqCst),
            name: name.to_string(),
            path: PathBuf::new(),
            bytes: 0,
            work: true,
            store: Mutex::new(store),
            rows: Vec::new(),
            broken: AtomicBool::new(false),
        });
        self.parts.write().unwrap().push(Arc::clone(&part));
        part
    }

    fn open_published(&self, path: &Path) -> Result<Part> {
        let store = Store::open_part(path)?;
        let rows = store.reading_rows()?;
        Ok(Part {
            slot: self.next_slot.fetch_add(1, Ordering::SeqCst),
            name: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            path: path.to_path_buf(),
            bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            work: false,
            store: Mutex::new(store),
            rows,
            broken: AtomicBool::new(false),
        })
    }

    /// Bring the pool in line with the folders: open parts that appeared,
    /// close parts that were merged away or failed. Returns whether anything
    /// changed; the caller then works out again which reading of each
    /// document is current ([`Pool::recover`]).
    pub fn refresh(&self) -> bool {
        let dirs = self.dirs.read().unwrap().clone();
        let mut on_disk: Vec<PathBuf> = Vec::new();
        let mut retired: HashMap<PathBuf, String> = HashMap::new();
        let mut listed_ok: HashSet<PathBuf> = HashSet::new();
        for dir in &dirs {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            listed_ok.insert(dir.clone());
            for e in rd.flatten() {
                let p = e.path();
                match p.extension().and_then(|x| x.to_str()) {
                    Some("db") => on_disk.push(p),
                    Some("retired") => {
                        if let Some(r) = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Retired>(&b).ok()) {
                            retired.insert(p.with_extension("db"), r.into);
                        }
                    }
                    _ => {}
                }
            }
        }
        on_disk.sort();
        let names: HashSet<String> = on_disk.iter().filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())).collect();
        let mut changed = false;
        let mut parts = self.parts.read().unwrap().clone();
        // Close: merged away (and its replacement is here), broken, or deleted.
        parts.retain(|p| {
            if p.work {
                return true;
            }
            let dir_listed = p.path.parent().is_some_and(|d| listed_ok.contains(d));
            let merged_away = retired.get(&p.path).is_some_and(|into| names.contains(into));
            let missing = dir_listed && !on_disk.contains(&p.path);
            let keep = !(merged_away || missing || p.broken.load(Ordering::SeqCst));
            changed |= !keep;
            keep
        });
        // Open what is new.
        for path in &on_disk {
            if parts.iter().any(|p| p.path == *path) {
                continue;
            }
            if retired.get(path).is_some_and(|into| names.contains(into)) {
                continue;
            }
            match self.open_published(path) {
                Ok(p) => {
                    self.refused.lock().unwrap().remove(path);
                    parts.push(Arc::new(p));
                    changed = true;
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    if self.refused.lock().unwrap().insert(path.clone(), why.clone()).as_ref() != Some(&why) {
                        warn!("part not used: {why}");
                    }
                }
            }
        }
        *self.parts.write().unwrap() = parts;
        if changed {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        changed
    }

    /// Reasons parts were refused (shown in the monitor).
    pub fn refused(&self) -> Vec<String> {
        self.refused.lock().unwrap().values().cloned().collect()
    }

    /// Work out the current reading of every path and hide the others.
    pub fn recover(&self, presence: &dyn Fn(&str) -> Presence) {
        let parts = self.parts();
        let mut all: Vec<(Arc<Part>, Vec<ReadingRow>)> = Vec::new();
        for p in &parts {
            let rows = if p.work { p.with(|s| s.reading_rows()).unwrap_or_default() } else { p.rows.clone() };
            all.push((Arc::clone(p), rows));
        }
        let mut cover: HashMap<u64, Covered> = HashMap::new();
        for (p, rows) in &all {
            for r in rows {
                let c = Covered { slot: p.slot, id: r.id, size: r.size, mtime: r.mtime, state: r.state, read_at: r.read_at };
                let key = path_key(&r.path);
                match cover.get(&key) {
                    // Newest reading wins; this PC's unpublished one on a tie.
                    Some(old) if old.read_at > c.read_at || (old.read_at == c.read_at && !p.work) => {}
                    _ => {
                        cover.insert(key, c);
                    }
                }
            }
        }
        let mut superseded: HashMap<u32, HashSet<i64>> = HashMap::new();
        for (p, rows) in &all {
            if p.work {
                continue;
            }
            let mut hidden: Vec<i64> = Vec::new();
            for r in rows {
                let current = cover.get(&path_key(&r.path)).is_some_and(|c| c.slot == p.slot && c.id == r.id);
                if !current {
                    superseded.entry(p.slot).or_default().insert(r.id);
                }
                // Waiting for OCR is not a reading: nothing to show.
                if !current || r.state == state::NEEDS_OCR || presence(&r.path) == Presence::Gone {
                    hidden.push(r.id);
                }
            }
            if let Err(e) = p.with(|s| s.set_hidden(&hidden)) {
                warn!("part {}: {e:#}", p.name);
            }
        }
        *self.superseded.write().unwrap() = superseded;
        // A reading of a file that is gone covers nothing.
        cover.retain(|_, c| {
            let Some((p, rows)) = all.iter().find(|(p, _)| p.slot == c.slot) else { return false };
            p.work || rows.iter().find(|r| r.id == c.id).is_none_or(|r| presence(&r.path) != Presence::Gone)
        });
        *self.cover.write().unwrap() = cover;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Record a reading just saved to `work` without recomputing everything.
    pub fn note_saved(&self, work: &Part, path: &str, c: Covered) {
        debug_assert!(work.work);
        self.cover.write().unwrap().insert(path_key(path), c);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// The current reading of a path, if any.
    pub fn covered(&self, path: &str) -> Option<Covered> {
        self.cover.read().unwrap().get(&path_key(path)).copied()
    }

    /// How a file stands: read, failed, waiting for OCR, or (None) still to
    /// open. A file whose size or date changed since is to be read again.
    pub fn state_of(&self, path: &str, size: u64, mtime: i64) -> Option<i64> {
        self.state_of_key(path_key(path), size, mtime)
    }

    /// [`Pool::state_of`] by a key already worked out ([`PathKeyer`]).
    pub fn state_of_key(&self, key: u64, size: u64, mtime: i64) -> Option<i64> {
        self.cover.read().unwrap().get(&key).filter(|c| c.size == size && c.mtime == mtime).map(|c| c.state)
    }

    /// Whether a newer reading of the same path replaced this one.
    pub fn is_superseded(&self, slot: u32, id: i64) -> bool {
        self.superseded.read().unwrap().get(&slot).is_some_and(|s| s.contains(&id))
    }

    /// The published parts in `dir`.
    pub fn parts_in(&self, dir: &Path) -> Vec<Arc<Part>> {
        self.parts.read().unwrap().iter().filter(|p| !p.work && p.path.parent() == Some(dir)).cloned().collect()
    }

    /// Run `f` on the part holding the current reading of `path`.
    pub fn with_file<T>(&self, path: &str, f: impl FnOnce(&Store, i64) -> Result<T>) -> Result<Option<T>> {
        let Some(c) = self.covered(path) else { return Ok(None) };
        let Some(p) = self.part(c.slot) else { return Ok(None) };
        p.with(|s| f(s, c.id)).map(Some)
    }

    /// Run `f` on every part at once and collect what each returns. A part
    /// that fails is logged and left out: the others still answer.
    pub fn each<T: Send>(&self, f: impl Fn(&Part, &Store) -> Result<T> + Sync) -> Vec<(Arc<Part>, T)> {
        let parts = self.parts();
        let run = |p: &Arc<Part>| match p.with(|s| f(p, s)) {
            Ok(v) => Some((Arc::clone(p), v)),
            Err(e) => {
                warn!("search part {}: {e:#}", p.name);
                None
            }
        };
        if parts.len() <= 1 {
            return parts.iter().filter_map(run).collect();
        }
        std::thread::scope(|sc| {
            let handles: Vec<_> = parts.iter().map(|p| sc.spawn(|| run(p))).collect();
            handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
        })
    }

    pub fn stats(&self) -> PoolStats {
        let cover = self.cover.read().unwrap();
        let parts = self.parts.read().unwrap();
        PoolStats {
            parts: parts.iter().filter(|p| !p.work).count() as u32,
            bytes: parts.iter().map(|p| p.bytes).sum(),
            documents: cover.values().filter(|c| c.state == state::READ).count() as u64,
            failed: cover.values().filter(|c| c.state == state::FAILED).count() as u64,
        }
    }
}

fn safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect()
}

fn unique() -> u32 {
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    (t ^ std::process::id().rotate_left(16)).wrapping_add(N.fetch_add(1, Ordering::SeqCst)) & 0xFFFF
}

/// Publish `work`'s readings as a new part in `dir`: a compact copy written
/// under a temporary name, checked, then renamed. Returns the part's path,
/// or `None` when there was nothing to publish. `work` is not changed: the
/// caller empties it once the pool has opened the part.
pub fn publish(work: &Store, dir: &Path, pc: &str) -> Result<Option<PathBuf>> {
    let (read, failed) = work.reading_counts()?;
    if read + failed == 0 {
        return Ok(None);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = format!("p-{:011}-{}-{:04x}", now(), safe(pc), unique());
    let tmp = dir.join(format!("{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let _ = work.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    work.conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()]).with_context(|| format!("write {}", tmp.display()))?;
    let check = || -> Result<()> {
        let copy = Store::open_part(&tmp)?;
        let ok: String = copy.conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
        if ok != "ok" {
            bail!("the copy failed its check: {ok}");
        }
        if copy.reading_counts()? != (read, failed) {
            bail!("the copy does not hold every reading");
        }
        Ok(())
    };
    if let Err(e) = check() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context("publishing a part"));
    }
    let dest = dir.join(format!("{name}.db"));
    std::fs::rename(&tmp, &dest).with_context(|| format!("publish {}", dest.display()))?;
    info!("published {read} reading(s) and {failed} failure(s) as {name}");
    Ok(Some(dest))
}

/// Which parts to merge now, if any: all the small ones once there are
/// enough of them, so the pool stays at a couple of dozen parts however
/// many PCs publish.
pub fn merge_candidates(parts: &[Arc<Part>]) -> Vec<Arc<Part>> {
    const TIERS: [(u64, usize); 3] = [(32 << 20, 8), (256 << 20, 6), (BIGGEST_MERGED, 4)];
    let mut lower = 0u64;
    for (upper, enough) in TIERS {
        let tier: Vec<Arc<Part>> = parts.iter().filter(|p| !p.work && p.bytes >= lower && p.bytes < upper).cloned().collect();
        if tier.len() >= enough {
            return tier;
        }
        lower = upper;
    }
    Vec::new()
}

/// Merge `sources` into one new part in `dir`, leaving out readings that a
/// newer one replaced (`current` says which are current). Readings of files
/// that are gone are kept: hiding them is undone if the file comes back, a
/// merge could not be. `keep_going` is asked between documents (it renews
/// the caller's lease); false abandons the merge. Returns the new part.
pub fn merge(sources: &[Arc<Part>], current: &dyn Fn(u32, i64) -> bool, dir: &Path, pc: &str, deriver: &Deriver, keep_going: &dyn Fn() -> bool) -> Result<Option<PathBuf>> {
    if sources.len() < 2 {
        return Ok(None);
    }
    let name = format!("m-{:011}-{}-{:04x}", now(), safe(pc), unique());
    let tmp = dir.join(format!("{name}.tmp"));
    let mut kept = 0u64;
    let built = (|| -> Result<bool> {
        let mut target = Store::create_bulk(&tmp)?;
        for src in sources {
            // Its own connection: every reading is read, hidden or not.
            let from = Store::open_part(&src.path)?;
            for row in &src.rows {
                if !current(src.slot, row.id) {
                    continue;
                }
                target.import_reading(&from, row.id, deriver)?;
                kept += 1;
                if kept.is_multiple_of(20) && !keep_going() {
                    return Ok(false);
                }
            }
        }
        target.set_meta("merged_from", &serde_json::to_string(&sources.iter().map(|s| s.name.clone()).collect::<Vec<_>>())?)?;
        target.conn.execute_batch("PRAGMA optimize;")?;
        Ok(true)
    })();
    match built {
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_file(&tmp);
            return Ok(None);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.context("merging parts"));
        }
    }
    let dest = dir.join(format!("{name}.db"));
    std::fs::rename(&tmp, &dest)?;
    for src in sources {
        let marker = src.path.with_extension("retired");
        let body = serde_json::to_vec(&Retired { into: name.clone(), at: now() })?;
        if let Err(e) = std::fs::write(&marker, body) {
            warn!("could not mark {} as merged: {e}", src.name);
        }
    }
    info!("merged {} parts into {name} ({kept} readings)", sources.len());
    Ok(Some(dest))
}

/// Delete parts merged away more than a day ago, and stale temporary files.
/// A part some PC still has open cannot be deleted yet; it goes next time.
pub fn sweep(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let old = |p: &Path, secs: u64| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a.as_secs() > secs);
    for e in rd.flatten() {
        let p = e.path();
        match p.extension().and_then(|x| x.to_str()) {
            Some("retired") if old(&p, RETIRED_KEEP_SECS) => {
                let db = p.with_extension("db");
                if !db.exists() || std::fs::remove_file(&db).is_ok() {
                    let _ = std::fs::remove_file(&p);
                }
            }
            Some("tmp") if old(&p, RETIRED_KEEP_SECS) => {
                let _ = std::fs::remove_file(&p);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{FileText, Line, PageText};
    use sleuth_core::protocol::{NormBox, TextSource};
    use sleuth_core::settings::Settings;

    fn scratch(name: &str) -> PathBuf {
        let base = std::env::var_os("SLEUTH_TEST_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("sl-pool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn save(s: &mut Store, path: &str, text: &str, at: i64) -> i64 {
        let d = Deriver::new(&Settings::default());
        let (dir, name) = path.rsplit_once('\\').unwrap();
        let (id, _) = s.upsert_file(path, dir, name, 10, 5, 0, state::WAITING).unwrap();
        let line = Line { text: text.into(), conf: 1.0, source: TextSource::TextLayer, bbox: NormBox::default(), alt: false };
        s.save_read(id, &format!("h-{text}"), &FileText { reader_version: 2, pages: vec![PageText { page: 1, lines: vec![line] }], ..Default::default() }, at, &d).unwrap();
        id
    }

    fn lines_of(pool: &Pool, path: &str) -> Vec<String> {
        pool.with_file(path, |s, id| Ok(s.file_text(id)?.pages.into_iter().flat_map(|p| p.lines).map(|l| l.text).collect())).unwrap().unwrap_or_default()
    }

    #[test]
    fn publish_then_search_newest_wins_and_merge_keeps_it() {
        let dir = scratch("flow");
        let text = dir.join("text one");
        let here = |_: &str| Presence::Unknown;
        let pool = Pool::new();
        pool.set_dirs(vec![text.clone()]);
        let work_path = dir.join("work.db");
        let mut work = Store::open(&work_path).unwrap();
        pool.add_work(&work_path).unwrap();

        save(&mut work, r"\\10.0.0.5\p\a.pdf", "PSV-1203", 100);
        save(&mut work, r"\\10.0.0.5\p\b.pdf", "FV-2201", 100);
        assert!(publish(&work, &text, "PC-ONE").unwrap().is_some());
        assert!(pool.refresh());
        work.clear_readings().unwrap();
        pool.recover(&here);
        assert_eq!(pool.stats().documents, 2);
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\P\A.pdf"), vec!["PSV-1203"], "paths compare without case");

        // The same file read again later: the new reading replaces the old.
        save(&mut work, r"\\10.0.0.5\p\a.pdf", "PSV-1204", 200);
        pool.recover(&here);
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\p\a.pdf"), vec!["PSV-1204"]);
        assert!(publish(&work, &text, "PC-ONE").unwrap().is_some());
        pool.refresh();
        work.clear_readings().unwrap();
        pool.recover(&here);
        assert_eq!(pool.stats().parts, 2);
        assert_eq!(pool.stats().documents, 2);
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\p\a.pdf"), vec!["PSV-1204"]);
        // The old reading is hidden inside its part: no query sees it.
        let seen: usize = pool.each(|_, s| Ok(s.conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))? as usize)).into_iter().map(|(_, n)| n).sum();
        assert_eq!(seen, 2);

        // Merge both parts: one part, the same answers, sources retired.
        let sources: Vec<Arc<Part>> = pool.parts().into_iter().filter(|p| !p.work).collect();
        let d = Deriver::new(&Settings::default());
        let current = |slot: u32, id: i64| !pool.is_superseded(slot, id);
        assert!(merge(&sources, &current, &text, "PC-ONE", &d, &|| true).unwrap().is_some());
        pool.refresh();
        pool.recover(&here);
        assert_eq!(pool.stats().parts, 1);
        assert_eq!(pool.stats().documents, 2);
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\p\a.pdf"), vec!["PSV-1204"]);
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\p\b.pdf"), vec!["FV-2201"]);

        // A file that is gone is hidden, and counted as not read.
        pool.recover(&|p: &str| if p.ends_with("b.pdf") { Presence::Gone } else { Presence::Here });
        assert_eq!(pool.stats().documents, 1);
        assert!(pool.covered(r"\\10.0.0.5\p\b.pdf").is_none());
        drop(pool);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn changed_file_is_not_covered() {
        let dir = scratch("cov");
        let pool = Pool::new();
        let work_path = dir.join("work.db");
        let mut work = Store::open(&work_path).unwrap();
        pool.add_work(&work_path).unwrap();
        save(&mut work, r"C:\w\a.pdf", "X-1", 100);
        pool.recover(&|_| Presence::Here);
        assert_eq!(pool.state_of(r"C:\w\a.pdf", 10, 5), Some(state::READ));
        assert_eq!(pool.state_of(r"C:\w\a.pdf", 11, 5), None, "another size: read again");
        assert_eq!(pool.state_of(r"C:\w\other.pdf", 10, 5), None);
        drop(pool);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keys_by_folder_and_name_match_keys_by_path() {
        let mut k = PathKeyer::default();
        assert_eq!(k.key(r"\\10.0.0.5\P\Iso", "A.pdf"), path_key(r"\\10.0.0.5\p\iso\a.PDF"));
        assert_eq!(k.key(r"\\10.0.0.5\P\Iso", "b.pdf"), path_key(r"\\10.0.0.5\p\iso\b.pdf"));
        assert_eq!(k.key(r"C:\", "x.txt"), path_key(r"C:\x.txt"));
    }

    #[test]
    fn ids_carry_their_part() {
        let id = compose(7, 123_456);
        assert_eq!(split(id), (7, 123_456));
        assert_eq!(split(compose(1, 1)), (1, 1));
    }

    #[test]
    fn merge_waits_for_enough_small_parts() {
        let mk = |bytes: u64| Arc::new(Part { slot: 1, name: "x".into(), path: PathBuf::new(), bytes, work: false, store: Mutex::new(Store::open_in_memory().unwrap()), rows: Vec::new(), broken: AtomicBool::new(false) });
        let few: Vec<Arc<Part>> = (0..7).map(|_| mk(1 << 20)).collect();
        assert!(merge_candidates(&few).is_empty());
        let enough: Vec<Arc<Part>> = (0..8).map(|_| mk(1 << 20)).chain([mk(3 << 30)]).collect();
        assert_eq!(merge_candidates(&enough).len(), 8, "the big part is left alone");
    }
}
