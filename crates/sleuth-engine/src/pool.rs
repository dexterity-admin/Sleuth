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
//! - Parts are searched from a copy on this PC's own disk (the *mirror*,
//!   see [`Pool::set_mirror`]). A part never changes, so a copy of the same
//!   size is the part. Searching hundreds of SQLite files over the network
//!   took minutes to open and failed whenever the share was busy (409 parts,
//!   66 GB, 23 Nov 2026: no text result 7 minutes after a start); from an SSD
//!   it takes seconds, and it still works while the share is away. A part
//!   that cannot be copied (no room, mirror switched off) is searched over
//!   the network as before.

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
/// Parts above this size are never merged again by a desktop PC; the office
/// server, which is always on, goes on to [`BIGGEST_MERGED_SERVER`].
const BIGGEST_MERGED: u64 = 1 << 30;
const BIGGEST_MERGED_SERVER: u64 = 4 << 30;
/// One merge takes at most this many parts and this many bytes, so it
/// ends in minutes. Taking every small part at once (182 of them on a busy
/// office, 12 Nov 2026) made merges that ran for hours over the network and
/// were cut off by PCs being restarted, so the parts were never merged and
/// every search had to look through all of them.
const MERGE_MAX_PARTS: usize = 16;
/// Parts opened at the same time by [`Pool::refresh`].
const OPEN_AT_ONCE: usize = 16;
/// Threads one query uses over all parts ([`Pool::each`]).
pub(crate) const QUERY_THREADS: usize = 16;
/// Room left on the mirror's disk whatever the office index grows to.
const MIRROR_KEEP_FREE: u64 = 8 << 30;
/// A part that could not be copied this many times is searched over the
/// network instead, so its documents are not missing from every search.
const MIRROR_TRIES: u32 = 3;
/// A copy in progress ends in this; a finished one is renamed to `.db`.
const COPYING_EXT: &str = "copying";

/// A part waiting to be copied to this PC.
#[derive(Debug, Clone)]
struct Pending {
    path: PathBuf,
    bytes: u64,
}

#[derive(Default)]
struct Mirror {
    /// The folder parts are copied to; `None` searches over the network.
    dir: Option<PathBuf>,
    /// Newest first.
    pending: Vec<Pending>,
    /// Parts being copied right now, by office path.
    in_flight: HashSet<PathBuf>,
    failed: HashMap<PathBuf, u32>,
    /// Why parts are not being copied, when they are not.
    note: String,
    /// An office folder could not be listed at the last refresh.
    away: bool,
}

/// How the copy on this PC stands, for the status.
#[derive(Debug, Clone, Default)]
pub struct MirrorStats {
    pub dir: String,
    /// Parts searched from this PC's disk, and their size.
    pub parts: u32,
    pub bytes: u64,
    /// Parts searched over the network (no room, or copying failed).
    pub remote_parts: u32,
    /// Parts still to copy.
    pub pending_parts: u32,
    pub pending_bytes: u64,
    /// Copied since the worker started.
    pub copied_bytes: u64,
    pub note: String,
    /// The office folder is away: only the copy is searched.
    pub away: bool,
}

/// The number in a part's name (`p-01795427712-pc-a95a`): when it was made.
fn part_time(path: &Path) -> u64 {
    path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.split('-').nth(1)).and_then(|t| t.parse().ok()).unwrap_or(0)
}

use crate::sys::free_bytes;

/// Copy `from` to `to` through a temporary name, giving up when `stop` is
/// set. The copy must be exactly `bytes` long and open as a part, or it is
/// thrown away.
fn copy_part(from: &Path, to: &Path, bytes: u64, stop: &AtomicBool, copied: &AtomicU64) -> Result<()> {
    use std::io::{Read, Write};
    let dir = to.parent().context("the copy has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    // Two Windows users on one PC may copy the same part at the same time.
    let tmp = to.with_extension(format!("{}-{COPYING_EXT}", std::process::id()));
    let run = || -> Result<()> {
        let mut src = std::fs::File::open(from).with_context(|| format!("open {}", from.display()))?;
        let mut dst = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let mut buf = vec![0u8; 1 << 20];
        let mut done = 0u64;
        loop {
            if stop.load(Ordering::SeqCst) {
                bail!("stopped");
            }
            let n = src.read(&mut buf).with_context(|| format!("read {}", from.display()))?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n]).with_context(|| format!("write {}", tmp.display()))?;
            done += n as u64;
            copied.fetch_add(n as u64, Ordering::Relaxed);
        }
        dst.flush()?;
        drop(dst);
        if done != bytes {
            bail!("{} is {done} bytes here and {bytes} in the office index", from.display());
        }
        drop(Store::open_part(&tmp)?);
        let _ = std::fs::remove_file(to);
        std::fs::rename(&tmp, to).with_context(|| format!("keep {}", to.display()))
    };
    let r = run();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

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

/// The lease a reader holds on a document while it reads it, by the
/// document's [`path_key`]: a sub-folder per first byte keeps the lease
/// folder's listings short.
pub fn doc_lease(key: u64) -> String {
    format!("doc\\{:02x}\\{:016x}", key >> 56, key)
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
    /// Read by a reader with a known fault that this document meets
    /// ([`ReadingRow::again`]): it stays searchable as it is, and counts as
    /// not read yet, so that it is read once more.
    pub again: bool,
    /// A newer look at the same file found that it needs OCR: the reading
    /// here stays searchable until the OCR is done, and the file stands in
    /// the OCR queue.
    pub wants_ocr: bool,
}

impl Covered {
    /// What covers a path when `new` was recorded after `old`. A later
    /// failure, or a later "needs OCR", does not take away what an earlier
    /// reading of the same file found (a document read once more because of
    /// a reader fault): the reading stays, a failure is not tried again, and
    /// a file that needs OCR goes on to it.
    fn after(new: Covered, old: Covered) -> Covered {
        if new.state != state::READ && old.state == state::READ && (new.size, new.mtime) == (old.size, old.mtime) {
            Covered { again: false, wants_ocr: new.state == state::NEEDS_OCR, ..old }
        } else {
            new
        }
    }
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
    /// Where the part lives in the office index: what it is known by.
    pub path: PathBuf,
    /// The file the queries read: the copy on this PC, or `path` itself.
    pub open_at: PathBuf,
    pub bytes: u64,
    /// This PC's unpublished readings (a live database, not a published part).
    pub work: bool,
    store: Mutex<Store>,
    /// What a published part covers (it never changes).
    rows: Rows,
    /// Set when a query failed on I/O: the part is reopened at the next refresh.
    broken: AtomicBool,
    /// What was last written to the part's list of hidden readings.
    hidden_sig: AtomicU64,
    /// Every reading in it is hidden (replaced by newer ones elsewhere):
    /// a text search has nothing to find here and would read the whole
    /// index of the part to learn that.
    all_hidden: AtomicBool,
}

/// What a published part covers, packed: a path shares its start with the
/// one before it, and that start is stored once. As a `Vec<ReadingRow>`
/// the 1.26 million readings of the office index took 295 MB of the
/// worker's memory (24 Nov 2026).
///
/// One entry: bytes shared with the previous path (u16), bytes that follow
/// (u32), those bytes, then id, size, modified time, read time (8 bytes
/// each, little-endian) and the state (one byte).
#[derive(Default)]
struct Rows {
    blob: Vec<u8>,
    n: u32,
}

impl Rows {
    fn pack(rows: &[ReadingRow]) -> Rows {
        let mut blob = Vec::new();
        let mut prev = "";
        for r in rows {
            let mut shared = prev.bytes().zip(r.path.bytes()).take(u16::MAX as usize).take_while(|(a, b)| a == b).count();
            while !r.path.is_char_boundary(shared) {
                shared -= 1;
            }
            let rest = &r.path.as_bytes()[shared..];
            blob.extend_from_slice(&(shared as u16).to_le_bytes());
            blob.extend_from_slice(&(rest.len() as u32).to_le_bytes());
            blob.extend_from_slice(rest);
            blob.extend_from_slice(&r.id.to_le_bytes());
            blob.extend_from_slice(&r.size.to_le_bytes());
            blob.extend_from_slice(&r.mtime.to_le_bytes());
            blob.extend_from_slice(&r.read_at.to_le_bytes());
            blob.push(r.state as u8 | if r.again { 0x80 } else { 0 });
            prev = &r.path;
        }
        blob.shrink_to_fit();
        Rows { blob, n: rows.len() as u32 }
    }

    /// Every row in the order it was packed.
    fn each(&self, f: &mut dyn FnMut(&ReadingRow)) {
        let b = &self.blob;
        let mut pos = 0usize;
        let mut path: Vec<u8> = Vec::new();
        let mut row = ReadingRow { id: 0, path: String::new(), size: 0, mtime: 0, state: 0, read_at: 0, error: String::new(), again: false };
        let i64_at = |at: usize| i64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        for _ in 0..self.n {
            let shared = u16::from_le_bytes([b[pos], b[pos + 1]]) as usize;
            let rest = u32::from_le_bytes(b[pos + 2..pos + 6].try_into().unwrap()) as usize;
            pos += 6;
            path.truncate(shared);
            path.extend_from_slice(&b[pos..pos + rest]);
            pos += rest;
            // Whole paths went in and were cut at character boundaries.
            row.path = String::from_utf8(std::mem::take(&mut path)).unwrap_or_default();
            row.id = i64_at(pos);
            row.size = i64_at(pos + 8) as u64;
            row.mtime = i64_at(pos + 16);
            row.read_at = i64_at(pos + 24);
            row.state = (b[pos + 32] & 0x7f) as i64;
            row.again = b[pos + 32] & 0x80 != 0;
            pos += 33;
            f(&row);
            path = std::mem::take(&mut row.path).into_bytes();
        }
    }

    fn ids(&self) -> Vec<i64> {
        let mut out = Vec::with_capacity(self.n as usize);
        self.each(&mut |r| out.push(r.id));
        out
    }
}

impl Part {
    /// Run a query on this part. An I/O failure (share gone) marks the part
    /// for reopening and is returned.
    pub fn with<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        let asked = std::time::Instant::now();
        let g = self.store.lock().unwrap_or_else(|p| p.into_inner());
        if asked.elapsed().as_millis() > 1000 {
            info!("part {}: {} waited {} ms for another job to let go of it", self.name, std::thread::current().name().unwrap_or("a request"), asked.elapsed().as_millis());
        }
        let r = f(&g);
        g.conn.flush_prepared_statement_cache();
        if let Err(e) = &r
            && is_io(e)
        {
            self.broken.store(true, Ordering::SeqCst);
        }
        r
    }

    /// Changes when other readings of this part are hidden.
    pub fn all_hidden(&self) -> bool {
        self.all_hidden.load(Ordering::SeqCst)
    }

    pub fn hidden_sig(&self) -> u64 {
        self.hidden_sig.load(Ordering::SeqCst)
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
    /// Paths whose file was gone when the file lists were last asked.
    gone: RwLock<HashSet<u64>>,
    next_slot: AtomicU32,
    /// Bumped whenever what a search can see changed.
    generation: AtomicU64,
    /// Bumped whenever a part was added or closed.
    parts_rev: AtomicU64,
    /// Parts refused (made by a newer Sleuth, damaged), with the reason, so
    /// each is reported once.
    refused: Mutex<HashMap<PathBuf, String>>,
    /// One [`Pool::refresh`] at a time.
    refresh_gate: Mutex<()>,
    mirror: Mutex<Mirror>,
    /// Bytes copied to the mirror since the worker started.
    copied: AtomicU64,
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

    /// Changes whenever a part was added or closed: the caller then works
    /// out again which reading of each document is current.
    pub fn parts_rev(&self) -> u64 {
        self.parts_rev.load(Ordering::SeqCst)
    }

    /// The folders published parts are looked for in.
    pub fn set_dirs(&self, dirs: Vec<PathBuf>) {
        *self.dirs.write().unwrap() = dirs;
    }

    /// A part failed on I/O (the office folder dropped) and waits to be
    /// opened again.
    pub fn any_broken(&self) -> bool {
        self.parts.read().unwrap().iter().any(|p| p.broken.load(Ordering::SeqCst))
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
            open_at: path.to_path_buf(),
            bytes: 0,
            work: true,
            store: Mutex::new(store),
            rows: Rows::default(),
            broken: AtomicBool::new(false),
            hidden_sig: AtomicU64::new(0),
            all_hidden: AtomicBool::new(false),
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
            open_at: PathBuf::new(),
            bytes: 0,
            work: true,
            store: Mutex::new(store),
            rows: Rows::default(),
            broken: AtomicBool::new(false),
            hidden_sig: AtomicU64::new(0),
            all_hidden: AtomicBool::new(false),
        });
        self.parts.write().unwrap().push(Arc::clone(&part));
        part
    }

    /// Open the part known as `path` from the file `open_at` (its copy on
    /// this PC, or `path` itself).
    fn open_published(&self, path: &Path, open_at: &Path) -> Result<Part> {
        let store = Store::open_part(open_at)?;
        let rows = Rows::pack(&store.reading_rows()?);
        Ok(Part {
            slot: self.next_slot.fetch_add(1, Ordering::SeqCst),
            name: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            path: path.to_path_buf(),
            open_at: open_at.to_path_buf(),
            bytes: std::fs::metadata(open_at).map(|m| m.len()).unwrap_or(0),
            work: false,
            store: Mutex::new(store),
            rows,
            broken: AtomicBool::new(false),
            hidden_sig: AtomicU64::new(0),
            all_hidden: AtomicBool::new(false),
        })
    }

    // ---- the copy on this PC -------------------------------------------------

    /// Search parts from copies in `dir` on this PC's own disk (`None`: over
    /// the network). Takes effect at the next [`Pool::refresh`].
    pub fn set_mirror(&self, dir: Option<PathBuf>) {
        let mut m = self.mirror.lock().unwrap();
        if m.dir != dir {
            m.dir = dir;
            m.pending.clear();
            m.failed.clear();
            m.note.clear();
        }
    }

    /// The mirror's folder for the office folder `dir`: the office's own
    /// parts and this PC's local-disk parts are kept apart.
    fn mirror_sub(mirror: &Path, dirs: &[PathBuf], dir: &Path) -> PathBuf {
        mirror.join(if dirs.first().map(|d| d.as_path()) == Some(dir) { "office" } else { "own" })
    }

    /// Where the copy of the part `path` is kept, when there is a mirror.
    fn local_for(&self, path: &Path) -> Option<PathBuf> {
        let mirror = self.mirror.lock().unwrap().dir.clone()?;
        let dirs = self.dirs.read().unwrap().clone();
        Some(Self::mirror_sub(&mirror, &dirs, path.parent()?).join(path.file_name()?))
    }

    /// Start searching a part, unless it is in the pool already.
    fn add_part(&self, part: Part) {
        let mut parts = self.parts.write().unwrap();
        if parts.iter().any(|p| !p.work && p.path == part.path) {
            return;
        }
        self.refused.lock().unwrap().remove(&part.path);
        parts.push(Arc::new(part));
        drop(parts);
        self.parts_rev.fetch_add(1, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Copy one part to this PC and start searching it. With no mirror, or
    /// no room on its disk, the part is searched where it is.
    fn bring(&self, job: &Pending, stop: &AtomicBool) -> Result<()> {
        let mut open_at = job.path.clone();
        if let Some(local) = self.local_for(&job.path) {
            let dir = local.parent().map(Path::to_path_buf).unwrap_or_default();
            let _ = std::fs::create_dir_all(&dir);
            if free_bytes(&dir) < job.bytes.saturating_add(MIRROR_KEEP_FREE) {
                let note = format!("Not enough room in {} for the whole office index; the rest is searched over the network (slower).", dir.display());
                let mut m = self.mirror.lock().unwrap();
                if m.note != note {
                    warn!("{note}");
                    m.note = note;
                }
            } else {
                copy_part(&job.path, &local, job.bytes, stop, &self.copied)?;
                open_at = local;
            }
        }
        let part = self.open_published(&job.path, &open_at)?;
        self.add_part(part);
        Ok(())
    }

    /// Copy the next waiting part to this PC and start searching it. False
    /// when nothing waits (or the office folder is away). The engine runs
    /// this on its mirror threads; `stop` abandons a copy in progress.
    pub fn mirror_step(&self, stop: &AtomicBool) -> bool {
        let job = {
            let mut m = self.mirror.lock().unwrap();
            if m.dir.is_none() || m.away || m.pending.is_empty() {
                return false;
            }
            let job = m.pending.remove(0);
            m.in_flight.insert(job.path.clone());
            job
        };
        let r = self.bring(&job, stop);
        let mut m = self.mirror.lock().unwrap();
        m.in_flight.remove(&job.path);
        match r {
            Ok(()) => {
                m.failed.remove(&job.path);
            }
            Err(_) if stop.load(Ordering::SeqCst) => m.pending.insert(0, job),
            // The office folder went away in the middle: no fault of the
            // part. It waits its turn again, and nothing more is tried
            // until the folder answers (counting these as failed copies
            // would leave every waiting part to be searched over the
            // network once the folder is back).
            Err(_) if job.path.parent().is_none_or(|d| std::fs::metadata(d).is_err()) => {
                m.pending.insert(0, job);
                m.away = true;
                return false;
            }
            Err(e) => {
                let n = {
                    let n = m.failed.entry(job.path.clone()).or_insert(0);
                    *n += 1;
                    *n
                };
                warn!("part not copied to this PC (try {n}): {e:#}");
                // Tried again after the others; then left to `refresh`, which
                // searches it over the network.
                if n < MIRROR_TRIES {
                    m.pending.push(job);
                }
            }
        }
        true
    }

    /// Have the part `path` (just published or merged by this PC) in the
    /// pool before returning, so no search misses its documents.
    pub fn adopt(&self, path: &Path) -> Result<()> {
        loop {
            if self.parts.read().unwrap().iter().any(|p| !p.work && p.path == path) {
                return Ok(());
            }
            let mut m = self.mirror.lock().unwrap();
            if m.in_flight.contains(path) {
                drop(m);
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            m.pending.retain(|j| j.path != path);
            m.in_flight.insert(path.to_path_buf());
            break;
        }
        let job = Pending { path: path.to_path_buf(), bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) };
        let r = self.bring(&job, &AtomicBool::new(false)).or_else(|e| {
            warn!("{}: not copied to this PC, searched over the network: {e:#}", job.path.display());
            self.open_published(path, path).map(|p| self.add_part(p))
        });
        self.mirror.lock().unwrap().in_flight.remove(path);
        r
    }

    /// (reading rows held in memory, bytes of their paths): for the log line
    /// that says where the worker's memory goes.
    pub fn row_stats(&self) -> (u64, u64) {
        let parts = self.parts.read().unwrap();
        let rows = parts.iter().map(|p| p.rows.n as u64).sum();
        let bytes = parts.iter().map(|p| p.rows.blob.len() as u64).sum();
        (rows, bytes)
    }

    /// How the copy on this PC stands.
    pub fn mirror_stats(&self) -> MirrorStats {
        let m = self.mirror.lock().unwrap();
        let mut st = MirrorStats {
            dir: m.dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default(),
            pending_parts: (m.pending.len() + m.in_flight.len()) as u32,
            pending_bytes: m.pending.iter().map(|j| j.bytes).sum(),
            copied_bytes: self.copied.load(Ordering::Relaxed),
            note: m.note.clone(),
            away: m.away,
            ..Default::default()
        };
        drop(m);
        for p in self.parts.read().unwrap().iter().filter(|p| !p.work) {
            if p.open_at == p.path {
                st.remote_parts += 1;
            } else {
                st.parts += 1;
                st.bytes += p.bytes;
            }
        }
        st
    }

    /// Bring the pool in line with the folders: open parts that appeared,
    /// close parts that were merged away or failed, and queue what is not on
    /// this PC yet for [`Pool::mirror_step`]. While the office folder is
    /// away, the parts already copied here are searched. Returns whether
    /// anything changed; the caller then works out again which reading of
    /// each document is current ([`Pool::recover`]).
    pub fn refresh(&self) -> bool {
        let _gate = self.refresh_gate.lock().unwrap_or_else(|p| p.into_inner());
        let dirs = self.dirs.read().unwrap().clone();
        let mirror = self.mirror.lock().unwrap().dir.clone();
        let local_dir = |dir: &Path| mirror.as_ref().map(|m| Self::mirror_sub(m, &dirs, dir));
        let mut on_disk: Vec<(PathBuf, u64)> = Vec::new();
        let mut retired: HashMap<PathBuf, String> = HashMap::new();
        let mut listed_ok: HashSet<PathBuf> = HashSet::new();
        let mut away = false;
        for (i, dir) in dirs.iter().enumerate() {
            match std::fs::read_dir(dir) {
                Ok(rd) => {
                    listed_ok.insert(dir.clone());
                    for e in rd.flatten() {
                        let p = e.path();
                        match p.extension().and_then(|x| x.to_str()) {
                            Some("db") => on_disk.push((p, e.metadata().map(|m| m.len()).unwrap_or(0))),
                            Some("retired") => {
                                if let Some(r) = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Retired>(&b).ok()) {
                                    retired.insert(p.with_extension("db"), r.into);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(_) => {
                    // The office's folder first: when it cannot be listed the
                    // share is away. This PC's own folder may just not exist yet.
                    if i == 0 {
                        away = true;
                    }
                    if !away {
                        continue;
                    }
                    // Away: what was copied to this PC is still searched.
                    let Some(rd) = local_dir(dir).and_then(|l| std::fs::read_dir(l).ok()) else { continue };
                    for e in rd.flatten() {
                        let p = e.path();
                        if p.extension().and_then(|x| x.to_str()) == Some("db")
                            && let Some(name) = p.file_name()
                        {
                            on_disk.push((dir.join(name), e.metadata().map(|m| m.len()).unwrap_or(0)));
                        }
                    }
                }
            }
        }
        // Newest first: what was read lately is what people look for.
        on_disk.sort_by(|a, b| part_time(&b.0).cmp(&part_time(&a.0)).then(a.0.cmp(&b.0)));
        let names: HashSet<String> = on_disk.iter().filter_map(|(p, _)| p.file_stem().map(|s| s.to_string_lossy().into_owned())).collect();
        let here: HashSet<&PathBuf> = on_disk.iter().map(|(p, _)| p).collect();
        let parts = self.parts.read().unwrap().clone();
        // Parts being searched now. A part merged into another is let go of
        // once that other one is among them, not as soon as it is published:
        // a merged part of a few GB takes minutes to reach this PC, and its
        // documents would be missing from every search until then.
        let live: HashSet<String> = parts.iter().filter(|p| !p.work).map(|p| p.name.clone()).collect();
        // Whether a part's readings are now in one of `set`, following merges
        // of merges (a part merged into one that was itself merged later).
        // Looking one step only opened 150 replaced parts again, and a search
        // read each of them to the end for rows that are all hidden
        // (full pass of a tag: 2 minutes instead of 4 s, 24 Nov 2026).
        let replaced_in = |path: &Path, set: &HashSet<String>| -> bool {
            let mut at = path.to_path_buf();
            for _ in 0..32 {
                let Some(into) = retired.get(&at) else { return false };
                if set.contains(into) {
                    return true;
                }
                at = at.with_file_name(format!("{into}.db"));
            }
            false
        };
        // Close: merged away (and its replacement is searched), broken, or deleted.
        let mut closed: HashSet<u32> = HashSet::new();
        // Copies that failed while being searched: copied again.
        let mut discard: HashSet<PathBuf> = HashSet::new();
        for p in parts.iter().filter(|p| !p.work) {
            let dir_listed = p.path.parent().is_some_and(|d| listed_ok.contains(d));
            let merged_away = replaced_in(&p.path, &live);
            let missing = dir_listed && !here.contains(&p.path);
            let broken = p.broken.load(Ordering::SeqCst);
            // The copy was moved to another folder (or switched off): the
            // part is opened again from where it belongs now.
            let stray = p.open_at != p.path && mirror.as_ref().is_none_or(|m| !p.open_at.starts_with(m));
            if merged_away || missing || broken || stray {
                closed.insert(p.slot);
                if broken && p.open_at != p.path {
                    warn!("the copy of part {} on this PC failed; it is copied again", p.name);
                    discard.insert(p.open_at.clone());
                }
            }
        }
        // The copy is opened where it is told to be, not where it was.
        let parts: Vec<Arc<Part>> = parts.into_iter().filter(|p| p.work || !closed.contains(&p.slot)).collect();
        let (in_flight, failed) = {
            let m = self.mirror.lock().unwrap();
            (m.in_flight.clone(), m.failed.clone())
        };
        // New parts: from this PC's copy when it is whole, else queued to be
        // copied; over the network only without a mirror or after copying
        // failed for good.
        let mut to_open: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut pending: Vec<Pending> = Vec::new();
        let local_of = |path: &Path| path.parent().and_then(local_dir).zip(path.file_name()).map(|(d, n)| d.join(n));
        let whole_here = |local: &Path, bytes: u64| bytes > 0 && !discard.contains(local) && std::fs::metadata(local).is_ok_and(|m| m.len() == bytes);
        // Parts that can be searched after this round without copying.
        let ready: HashSet<String> = on_disk
            .iter()
            .filter(|(path, bytes)| local_of(path).is_none_or(|l| whole_here(&l, *bytes)))
            .filter_map(|(p, _)| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .chain(live.iter().cloned())
            .collect();
        for (path, bytes) in &on_disk {
            if in_flight.contains(path) || parts.iter().any(|p| !p.work && p.path == *path) {
                continue;
            }
            // Merged into another part: not needed once that one can be
            // searched, and never worth copying.
            let merged = replaced_in(path, &names);
            if merged && replaced_in(path, &ready) {
                continue;
            }
            let local = local_of(path);
            match local {
                None => to_open.push((path.clone(), path.clone())),
                Some(local) => {
                    let whole = whole_here(&local, *bytes);
                    if whole {
                        to_open.push((path.clone(), local));
                    } else if failed.get(path).is_some_and(|n| *n >= MIRROR_TRIES) {
                        to_open.push((path.clone(), path.clone()));
                    } else if !away && !merged {
                        pending.push(Pending { path: path.clone(), bytes: *bytes });
                    }
                }
            }
        }
        // Open many at once: one by one, a few hundred parts kept search
        // empty for minutes after a start.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let opened: Mutex<Vec<(PathBuf, Result<Part>)>> = Mutex::new(Vec::new());
        std::thread::scope(|sc| {
            for _ in 0..OPEN_AT_ONCE.min(to_open.len()) {
                sc.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        let Some((path, open_at)) = to_open.get(i) else { break };
                        let r = self.open_published(path, open_at);
                        opened.lock().unwrap().push((path.clone(), r));
                    }
                });
            }
        });
        let mut opened = opened.into_inner().unwrap();
        opened.sort_by(|a, b| a.0.cmp(&b.0));
        let mut changed = !closed.is_empty();
        let mut fresh: Vec<Arc<Part>> = Vec::new();
        for (path, r) in opened {
            match r {
                Ok(p) => {
                    self.refused.lock().unwrap().remove(&path);
                    fresh.push(Arc::new(p));
                }
                Err(e) => {
                    // A copy that does not open is copied again next time.
                    if let Some((_, open_at)) = to_open.iter().find(|(p, o)| *p == path && o != p) {
                        discard.insert(open_at.clone());
                    }
                    let why = format!("{e:#}");
                    if self.refused.lock().unwrap().insert(path.clone(), why.clone()).as_ref() != Some(&why) {
                        warn!("part not used: {why}");
                    }
                }
            }
        }
        {
            // Mirror threads may have added parts meanwhile: theirs stay.
            let mut live = self.parts.write().unwrap();
            live.retain(|p| !closed.contains(&p.slot));
            for p in fresh {
                if !live.iter().any(|q| !q.work && q.path == p.path) {
                    live.push(p);
                    changed = true;
                }
            }
            // A part whose replacement was opened just now goes with it.
            let now_live: HashSet<String> = live.iter().filter(|p| !p.work).map(|p| p.name.clone()).collect();
            let before = live.len();
            live.retain(|p| p.work || !replaced_in(&p.path, &now_live));
            changed |= live.len() != before;
        }
        {
            let mut m = self.mirror.lock().unwrap();
            m.away = away;
            let live = self.parts.read().unwrap();
            pending.retain(|j| !m.in_flight.contains(&j.path) && !live.iter().any(|p| !p.work && p.path == j.path));
            m.pending = pending;
            if m.pending.is_empty() && m.in_flight.is_empty() && !m.note.starts_with("Not enough room") {
                m.note.clear();
            }
        }
        // Tidy the mirror: copies of parts the office no longer has, failed
        // copies, and copies abandoned half-way more than an hour ago. (The
        // parts closed above are let go of first, or Windows keeps their files.)
        drop(parts);
        for f in &discard {
            let _ = std::fs::remove_file(f);
        }
        let live_now: HashSet<String> = self.parts.read().unwrap().iter().filter(|p| !p.work).map(|p| p.name.clone()).collect();
        // A copy of a part that was merged into one searched now is not
        // needed again (395 such copies held 17 GB on 24 Nov 2026).
        let merged_away = |p: &Path| {
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            !live_now.contains(&stem) && replaced_in(p, &live_now)
        };
        for dir in &listed_ok {
            let Some(rd) = local_dir(dir).and_then(|l| std::fs::read_dir(l).ok()) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
                let stale = if ext == "db" {
                    p.file_name().is_some_and(|n| !here.contains(&dir.join(n)) || merged_away(&dir.join(n)))
                } else {
                    ext.ends_with(COPYING_EXT) && e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a.as_secs() > 3600)
                };
                if stale {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        if changed {
            self.parts_rev.fetch_add(1, Ordering::SeqCst);
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
        self.recover_with(Some(presence));
    }

    /// As [`Pool::recover`] after the parts changed, without asking the file
    /// lists again: a file counts as gone when it was at the last
    /// [`Pool::recover`]. Asking the lists about every reading is most of
    /// the work (35 s of one core for 600,000 documents, 24 Nov 2026), and
    /// the lists do not change because a part arrived.
    pub fn recover_parts(&self) {
        self.recover_with(None);
    }

    fn recover_with(&self, presence: Option<&dyn Fn(&str) -> Presence>) {
        let was_gone = if presence.is_none() { self.gone.read().unwrap().clone() } else { HashSet::new() };
        let parts = self.parts();
        let started = std::time::Instant::now();
        let mut cover: HashMap<u64, Covered> = HashMap::new();
        for p in &parts {
            crate::sys::rest_while_searching();
            // A published part's rows never change; `work` is asked each time.
            let work_rows = if p.work { p.with(|s| s.reading_rows()).unwrap_or_default() } else { Vec::new() };
            let mut note = |r: &ReadingRow| {
                let c = Covered { slot: p.slot, id: r.id, size: r.size, mtime: r.mtime, state: r.state, read_at: r.read_at, again: r.again, wants_ocr: false };
                match cover.entry(path_key(&r.path)) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        // Newest reading wins; this PC's unpublished one on a tie.
                        let old = *e.get();
                        let (new, old) = if c.read_at > old.read_at || (c.read_at == old.read_at && p.work) { (c, old) } else { (old, c) };
                        e.insert(Covered::after(new, old));
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(c);
                    }
                }
            };
            if p.work {
                work_rows.iter().for_each(&mut note);
            } else {
                p.rows.each(&mut note);
            }
        }
        let mut superseded: HashMap<u32, HashSet<i64>> = HashMap::new();
        let mut gone: Vec<u64> = Vec::new();
        for p in &parts {
            if p.work {
                continue;
            }
            crate::sys::rest_while_searching();
            let mut hidden: Vec<i64> = Vec::new();
            p.rows.each(&mut |r: &ReadingRow| {
                let key = path_key(&r.path);
                let current = cover.get(&key).is_some_and(|c| c.slot == p.slot && c.id == r.id);
                if !current {
                    superseded.entry(p.slot).or_default().insert(r.id);
                    hidden.push(r.id);
                } else if match presence {
                    Some(ask) => ask(&r.path) == Presence::Gone,
                    None => was_gone.contains(&key),
                } {
                    gone.push(key);
                    hidden.push(r.id);
                } else if r.state == state::NEEDS_OCR {
                    // Waiting for OCR is not a reading: nothing to show.
                    hidden.push(r.id);
                }
            });
            p.all_hidden.store(p.rows.n > 0 && hidden.len() as u32 == p.rows.n, Ordering::SeqCst);
            // Written to the part only when it differs from last time.
            let sig = hidden.iter().fold(hidden.len() as u64, |a, id| a.wrapping_mul(1_000_003).wrapping_add(*id as u64));
            if p.hidden_sig.swap(sig, Ordering::SeqCst) != sig
                && let Err(e) = p.with(|s| s.set_hidden(&hidden))
            {
                warn!("part {}: {e:#}", p.name);
                p.hidden_sig.store(u64::MAX, Ordering::SeqCst);
            }
        }
        *self.superseded.write().unwrap() = superseded;
        // A reading of a file that is gone covers nothing.
        for key in &gone {
            cover.remove(key);
        }
        if presence.is_some() {
            *self.gone.write().unwrap() = gone.into_iter().collect();
        }
        *self.cover.write().unwrap() = cover;
        self.generation.fetch_add(1, Ordering::SeqCst);
        if started.elapsed().as_secs() >= 3 {
            info!("which readings are current took {} ms for {} parts{}", started.elapsed().as_millis(), parts.len(), if presence.is_some() { ", file lists asked" } else { "" });
        }
    }

    /// Record a reading just saved to `work` without recomputing everything.
    pub fn note_saved(&self, work: &Part, path: &str, c: Covered) {
        debug_assert!(work.work);
        let mut cover = self.cover.write().unwrap();
        let key = path_key(path);
        let c = match cover.get(&key) {
            Some(old) => Covered::after(c, *old),
            None => c,
        };
        cover.insert(key, c);
        drop(cover);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// The current reading of a path, if any.
    pub fn covered(&self, path: &str) -> Option<Covered> {
        self.cover.read().unwrap().get(&path_key(path)).copied()
    }

    /// Documents read at or after `t` (unix seconds) that this PC can see:
    /// the office's real output, whoever read them.
    pub fn read_since(&self, t: i64) -> u64 {
        self.cover.read().unwrap().values().filter(|c| c.state == state::READ && c.read_at >= t).count() as u64
    }

    /// How a file stands: read, failed, waiting for OCR, or (None) still to
    /// open. A file whose size or date changed since is to be read again.
    pub fn state_of(&self, path: &str, size: u64, mtime: i64) -> Option<i64> {
        self.state_of_key(path_key(path), size, mtime)
    }

    /// [`Pool::state_of`] by a key already worked out ([`PathKeyer`]).
    pub fn state_of_key(&self, key: u64, size: u64, mtime: i64) -> Option<i64> {
        self.cover.read().unwrap().get(&key).filter(|c| c.size == size && c.mtime == mtime && !c.again).map(|c| if c.wants_ocr { state::NEEDS_OCR } else { c.state })
    }

    /// As [`Pool::state_of_key`], with when that state was recorded.
    pub fn state_at_key(&self, key: u64, size: u64, mtime: i64) -> Option<(i64, i64)> {
        self.cover.read().unwrap().get(&key).filter(|c| c.size == size && c.mtime == mtime && !c.again).map(|c| (if c.wants_ocr { state::NEEDS_OCR } else { c.state }, c.read_at))
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
        self.each_on(QUERY_THREADS, f)
    }

    /// [`Pool::each_on`], or with `turns` one part after another, each in a
    /// background turn of its own ([`crate::sys::HalfTime`]). The turn is
    /// taken before the part is: a job that waited for its turn while it held
    /// a part kept every search out of that part for as long (the full pass
    /// of a search took 25 to 75 s instead of 4, 24 Nov 2026).
    pub fn each_in_turn<T: Send>(&self, turns: bool, threads: usize, f: impl Fn(&Part, &Store) -> Result<T> + Sync) -> Vec<(Arc<Part>, T)> {
        if !turns {
            return self.each_on(threads, f);
        }
        let mut out = Vec::new();
        for p in self.parts() {
            let _turn = crate::sys::HalfTime::start();
            match p.with(|s| f(&p, s)) {
                Ok(v) => out.push((Arc::clone(&p), v)),
                Err(e) => warn!("search part {}: {e:#}", p.name),
            }
        }
        out
    }

    /// [`Pool::each`] on at most `threads` threads.
    pub fn each_on<T: Send>(&self, threads: usize, f: impl Fn(&Part, &Store) -> Result<T> + Sync) -> Vec<(Arc<Part>, T)> {
        let parts = self.parts();
        let run = |p: &Arc<Part>| match p.with(|s| f(p, s)) {
            Ok(v) => Some((Arc::clone(p), v)),
            Err(e) => {
                warn!("search part {}: {e:#}", p.name);
                None
            }
        };
        if parts.len() <= 1 || threads <= 1 {
            return parts.iter().filter_map(run).collect();
        }
        // A few threads share the parts out: one thread per part was several
        // hundred threads for every word searched.
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|sc| {
            let handles: Vec<_> = (0..threads.min(parts.len()))
                .map(|_| {
                    sc.spawn(|| {
                        let mut out = Vec::new();
                        while let Some(p) = parts.get(next.fetch_add(1, Ordering::SeqCst)) {
                            out.extend(run(p));
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
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
///
/// Each tier is: parts below this size, how many of them make a merge worth
/// it, and the most one merge takes in. Until 1.4 no merge took in more than
/// 512 MB, so the parts stayed near that size and their number grew with the
/// index: 163 parts for 73 GB on 24 Nov 2026, and every search pays for each
/// part. The third tier now makes parts of up to 4 GB (about 20 for that
/// index); `server` adds a fourth, for the PC that is always on.
pub fn merge_candidates(parts: &[Arc<Part>], server: bool) -> Vec<Arc<Part>> {
    const TIERS: [(u64, usize, u64); 4] = [(32 << 20, 8, 512 << 20), (256 << 20, 6, 1 << 30), (BIGGEST_MERGED, 4, 4 << 30), (BIGGEST_MERGED_SERVER, 4, 16 << 30)];
    let mut lower = 0u64;
    for (upper, enough, max_bytes) in TIERS.into_iter().take(if server { 4 } else { 3 }) {
        let mut tier: Vec<Arc<Part>> = parts.iter().filter(|p| !p.work && p.bytes >= lower && p.bytes < upper).cloned().collect();
        if tier.len() >= enough {
            // Smallest first (oldest among equals), up to one merge's worth.
            tier.sort_by(|a, b| a.bytes.cmp(&b.bytes).then(a.name.cmp(&b.name)));
            let mut take = Vec::new();
            let mut bytes = 0u64;
            for p in tier {
                if take.len() >= MERGE_MAX_PARTS || (take.len() >= 2 && bytes + p.bytes > max_bytes) {
                    break;
                }
                bytes += p.bytes;
                take.push(p);
            }
            if take.len() >= 2 {
                return take;
            }
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
///
/// The new part is built in `build_dir` on this PC's own disk (thousands of
/// small writes are slow over the network) and then copied into `dir` under
/// a temporary name and renamed, as a published part is.
pub fn merge(sources: &[Arc<Part>], current: &dyn Fn(u32, i64) -> bool, dir: &Path, build_dir: &Path, pc: &str, deriver: &Deriver, keep_going: &dyn Fn() -> bool) -> Result<Option<PathBuf>> {
    if sources.len() < 2 {
        return Ok(None);
    }
    let name = format!("m-{:011}-{}-{:04x}", now(), safe(pc), unique());
    std::fs::create_dir_all(build_dir).with_context(|| format!("create {}", build_dir.display()))?;
    let tmp = build_dir.join(format!("{name}.building"));
    let mut kept = 0u64;
    let built = (|| -> Result<bool> {
        let mut target = Store::create_bulk(&tmp)?;
        for src in sources {
            // Its own connection: every reading is read, hidden or not.
            let from = Store::open_part(&src.open_at)?;
            for id in src.rows.ids() {
                if !current(src.slot, id) {
                    continue;
                }
                target.import_reading(&from, id, deriver)?;
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
    let upload = dir.join(format!("{name}.tmp"));
    let dest = dir.join(format!("{name}.db"));
    // In pieces, asking `keep_going` between them: a part of a few GB takes
    // minutes to send, and the caller's lease must not run out meanwhile.
    let copied = (|| -> Result<()> {
        use std::io::{Read as _, Write as _};
        let mut from = std::fs::File::open(&tmp)?;
        let mut to = std::fs::File::create(&upload)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut sent = 0usize;
        loop {
            let n = from.read(&mut buf)?;
            if n == 0 {
                break;
            }
            to.write_all(&buf[..n])?;
            sent += n;
            if sent >= 64 << 20 {
                sent = 0;
                anyhow::ensure!(keep_going(), "stopped while the merged part was being sent");
            }
        }
        to.sync_all()?;
        Ok(())
    })()
    .with_context(|| format!("copy the merged part to {}", upload.display()));
    let _ = std::fs::remove_file(&tmp);
    if let Err(e) = copied.and_then(|_| {
        // Checked where it landed, as a published part is.
        let copy = Store::open_part(&upload)?;
        let ok: String = copy.conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
        anyhow::ensure!(ok == "ok", "the merged part failed its check: {ok}");
        drop(copy);
        std::fs::rename(&upload, &dest).with_context(|| format!("publish {}", dest.display()))
    }) {
        let _ = std::fs::remove_file(&upload);
        return Err(e.context("merging parts"));
    }
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
        assert!(merge(&sources, &current, &text, &text.join("..").join("build"), "PC-ONE", &d, &|| true).unwrap().is_some());
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
    fn parts_are_searched_from_the_copy_on_this_pc() {
        let dir = scratch("mirror");
        let text = dir.join("office").join("text");
        let mirror = dir.join("copy");
        let here = |_: &str| Presence::Unknown;
        let stop = AtomicBool::new(false);
        let pool = Pool::new();
        pool.set_dirs(vec![text.clone(), dir.join("office").join("own")]);
        pool.set_mirror(Some(mirror.clone()));
        let work_path = dir.join("work.db");
        let mut work = Store::open(&work_path).unwrap();
        pool.add_work(&work_path).unwrap();

        // A part published by another PC: queued, copied, then searched here.
        save(&mut work, r"\\10.0.0.5\p\a.pdf", "PSV-1203", 100);
        let one = publish(&work, &text, "PC-TWO").unwrap().unwrap();
        work.clear_readings().unwrap();
        assert!(!pool.refresh(), "nothing is searched before it is copied");
        assert_eq!(pool.mirror_stats().pending_parts, 1);
        assert!(pool.mirror_step(&stop));
        assert!(!pool.mirror_step(&stop), "nothing left to copy");
        pool.recover(&here);
        let st = pool.mirror_stats();
        assert_eq!((st.parts, st.remote_parts, st.pending_parts), (1, 0, 0));
        let part = pool.parts().into_iter().find(|p| !p.work).unwrap();
        assert_eq!(part.path, one, "known by its place in the office index");
        assert!(part.open_at.starts_with(&mirror), "read from the copy");
        drop(part);
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\p\a.pdf"), vec!["PSV-1203"]);

        // This PC's own part is in the pool before `adopt` returns.
        save(&mut work, r"\\10.0.0.5\p\b.pdf", "FV-2201", 200);
        let two = publish(&work, &text, "PC-ONE").unwrap().unwrap();
        pool.adopt(&two).unwrap();
        work.clear_readings().unwrap();
        pool.recover(&here);
        assert_eq!(pool.mirror_stats().parts, 2);
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\p\b.pdf"), vec!["FV-2201"]);
        drop(pool);

        // A new start with the office folder away: the copy answers.
        std::fs::rename(dir.join("office"), dir.join("office-away")).unwrap();
        let pool = Pool::new();
        pool.set_dirs(vec![text.clone(), dir.join("office").join("own")]);
        pool.set_mirror(Some(mirror.clone()));
        assert!(pool.refresh());
        pool.recover(&here);
        let st = pool.mirror_stats();
        assert_eq!((st.parts, st.away), (2, true));
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\p\a.pdf"), vec!["PSV-1203"]);
        // Back again: nothing is copied twice, nothing closes.
        std::fs::rename(dir.join("office-away"), dir.join("office")).unwrap();
        assert!(!pool.refresh());
        assert_eq!((pool.mirror_stats().parts, pool.mirror_stats().pending_parts, pool.mirror_stats().away), (2, 0, false));

        // A part the office no longer has: closed, and its copy deleted.
        std::fs::remove_file(&one).unwrap();
        assert!(pool.refresh());
        pool.recover(&here);
        assert_eq!(pool.mirror_stats().parts, 1);
        assert!(!mirror.join("office").join(one.file_name().unwrap()).exists());
        assert!(pool.covered(r"\\10.0.0.5\p\a.pdf").is_none());

        // A copy cut short (the PC lost power while copying) is not trusted.
        let local = mirror.join("office").join(two.file_name().unwrap());
        drop(pool);
        std::fs::write(&local, b"half").unwrap();
        let pool = Pool::new();
        pool.set_dirs(vec![text.clone(), dir.join("office").join("own")]);
        pool.set_mirror(Some(mirror.clone()));
        pool.refresh();
        assert_eq!((pool.mirror_stats().parts, pool.mirror_stats().pending_parts), (0, 1));
        assert!(pool.mirror_step(&stop));
        pool.recover(&here);
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\p\b.pdf"), vec!["FV-2201"]);

        // The copy switched off: the same part, read where it is.
        pool.set_mirror(None);
        assert!(pool.refresh());
        pool.recover(&here);
        let st = pool.mirror_stats();
        assert_eq!((st.parts, st.remote_parts), (0, 1));
        assert_eq!(lines_of(&pool, r"\\10.0.0.5\p\b.pdf"), vec!["FV-2201"]);
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
    fn a_document_lease_is_named_by_its_key() {
        assert_eq!(doc_lease(0xab00_0000_0000_0001), r"doc\ab\ab00000000000001");
    }

    #[test]
    fn ids_carry_their_part() {
        let id = compose(7, 123_456);
        assert_eq!(split(id), (7, 123_456));
        assert_eq!(split(compose(1, 1)), (1, 1));
    }

    #[test]
    fn merge_waits_for_enough_small_parts() {
        let mk = |bytes: u64| Arc::new(Part { slot: 1, name: "x".into(), path: PathBuf::new(), open_at: PathBuf::new(), bytes, work: false, store: Mutex::new(Store::open_in_memory().unwrap()), rows: Rows::default(), broken: AtomicBool::new(false), hidden_sig: AtomicU64::new(0), all_hidden: AtomicBool::new(false) });
        let few: Vec<Arc<Part>> = (0..7).map(|_| mk(1 << 20)).collect();
        assert!(merge_candidates(&few, false).is_empty());
        let enough: Vec<Arc<Part>> = (0..8).map(|_| mk(1 << 20)).chain([mk(3 << 30)]).collect();
        assert_eq!(merge_candidates(&enough, false).len(), 8, "the big part is left alone");
        // Middle-sized parts are merged into one of a few GB; only the
        // server merges those again.
        let middle: Vec<Arc<Part>> = (0..5).map(|_| mk(400 << 20)).chain([mk(3 << 30), mk(3 << 30), mk(3 << 30), mk(3 << 30)]).collect();
        assert_eq!(merge_candidates(&middle, false).len(), 5);
        let large: Vec<Arc<Part>> = (0..4).map(|_| mk(3 << 30)).collect();
        assert!(merge_candidates(&large, false).is_empty());
        assert_eq!(merge_candidates(&large, true).len(), 4);
    }
}
