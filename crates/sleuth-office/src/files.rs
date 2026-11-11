//! The catalog of one root on disk: a folder of write-once files.
//!
//! The PC that walks a share publishes what it lists as numbered segments
//! (`seg-<pass>-<seq>-<pc>.cat`) and, now and then, the whole root as one
//! file (`full-<pass>-<seq>.cat`, the catalog after that segment). Every
//! other PC loads the newest full file and follows the segments after it.
//! A file gets its name only when it is complete (temporary name, flushed,
//! renamed) and is never touched again, so a reader on another PC needs no
//! lock and never sees half a file.
//!
//! Inside, gzip: one line of JSON (the header) and then the folder
//! listings, each prefixed by its length, with paths relative to the root.
//! The header says how many listings and files follow; a file that does
//! not deliver them, or fails the gzip checksum, is not used at all.

use crate::catalog::{Catalog, Entries, Listing, Tree, lower, norm_dir, rel_of};
use crate::fsx::{now, replace, retry, safe_name, this_pc};
use crate::wire::{get_str, get_varint, put_str, put_varint, unzigzag, zigzag};
use anyhow::{Context, Result, anyhow, ensure};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const FORMAT: u32 = 1;
/// Replaced files stay this long, for PCs that are still reading them.
const KEEP: Duration = Duration::from_secs(3600);
/// No folder listing is this large; a longer one is damage.
const MAX_RECORD: u64 = 1 << 28;

/// A place in a root's history: after segment `seq` of walk pass `pass`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Mark {
    pub pass: i64,
    pub seq: u32,
}

/// `state.json`: who walks the root and how far it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RootState {
    pub root: String,
    pub pass: i64,
    /// `walking`, `done`, `partial` or `offline`.
    pub phase: String,
    pub by: String,
    pub started: i64,
    pub updated: i64,
    pub last_ok: i64,
    pub folders: u64,
    pub files: u64,
    pub skipped: u64,
    pub note: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Header {
    format: u32,
    root: String,
    pass: i64,
    seq: u32,
    created: i64,
    by: String,
    dirs: u64,
    files: u64,
}

/// One catalog file by its name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    full: bool,
    mark: Mark,
    pc: String,
    name: String,
}

fn parse_name(name: &str) -> Option<Entry> {
    let stem = name.strip_suffix(".cat")?;
    if let Some(rest) = stem.strip_prefix("full-") {
        let (pass, seq) = rest.rsplit_once('-')?;
        return Some(Entry { full: true, mark: Mark { pass: pass.parse().ok()?, seq: seq.parse().ok()? }, pc: String::new(), name: name.into() });
    }
    let rest = stem.strip_prefix("seg-")?;
    // The pass may be negative, and the PC name may hold hyphens.
    let cut = rest.char_indices().skip(1).find(|(_, c)| *c == '-')?.0;
    let (seq, pc) = rest[cut + 1..].split_once('-')?;
    Some(Entry { full: false, mark: Mark { pass: rest[..cut].parse().ok()?, seq: seq.parse().ok()? }, pc: pc.into(), name: name.into() })
}

/// One listing as it lies in a file.
struct Rec<'a> {
    rel: &'a str,
    pass: i64,
    listed_at: i64,
    subdirs: Vec<&'a str>,
    files: Vec<(&'a str, u64, i64)>,
}

fn parse_record(b: &[u8]) -> Option<Rec<'_>> {
    let mut p = 0;
    let rel = get_str(b, &mut p)?;
    let pass = unzigzag(get_varint(b, &mut p)?);
    let listed_at = unzigzag(get_varint(b, &mut p)?);
    // Every name takes at least a byte, so a count beyond the record is damage, not a reason to allocate.
    let n = usize::try_from(get_varint(b, &mut p)?).ok().filter(|&n| n <= b.len())?;
    let mut subdirs = Vec::with_capacity(n);
    for _ in 0..n {
        subdirs.push(get_str(b, &mut p)?);
    }
    let n = usize::try_from(get_varint(b, &mut p)?).ok().filter(|&n| n <= b.len())?;
    let mut files = Vec::with_capacity(n);
    for _ in 0..n {
        files.push((get_str(b, &mut p)?, get_varint(b, &mut p)?, unzigzag(get_varint(b, &mut p)?)));
    }
    (p == b.len()).then_some(Rec { rel, pass, listed_at, subdirs, files })
}

/// Start a record in `body`; the subfolder and file counts and entries follow.
fn begin_record(body: &mut Vec<u8>, rel: &str, pass: i64, listed_at: i64) {
    body.clear();
    put_str(body, rel);
    put_varint(body, zigzag(pass));
    put_varint(body, zigzag(listed_at));
}

fn put_file(body: &mut Vec<u8>, name: &str, size: u64, mtime: i64) {
    put_str(body, name);
    put_varint(body, size);
    put_varint(body, zigzag(mtime));
}

/// The next record length; `None` at the end of the file.
fn read_len(r: &mut impl Read) -> Result<Option<u64>> {
    let (mut v, mut shift, mut b) = (0u64, 0u32, [0u8; 1]);
    loop {
        if r.read(&mut b)? == 0 {
            ensure!(shift == 0, "ends inside a record");
            return Ok(None);
        }
        ensure!(shift <= 63, "damaged record length");
        v |= ((b[0] & 0x7f) as u64) << shift;
        if b[0] < 0x80 {
            return Ok(Some(v));
        }
        shift += 7;
    }
}

/// Every record of a catalog file after its header. Returns how many
/// listings and files there were.
fn records(r: &mut impl Read, each: &mut dyn FnMut(Rec<'_>)) -> Result<(u64, u64)> {
    let (mut dirs, mut files) = (0u64, 0u64);
    let mut body = Vec::new();
    while let Some(n) = read_len(r)? {
        ensure!(n <= MAX_RECORD, "damaged record length");
        body.resize(n as usize, 0);
        r.read_exact(&mut body)?;
        let rec = parse_record(&body).ok_or_else(|| anyhow!("damaged record"))?;
        dirs += 1;
        files += rec.files.len() as u64;
        each(rec);
    }
    Ok((dirs, files))
}

fn check(head: &Header, got: (u64, u64)) -> Result<()> {
    ensure!(got == (head.dirs, head.files), "holds {} folders and {} files where its header says {} and {}", got.0, got.1, head.dirs, head.files);
    Ok(())
}

/// Apply the checked records of a segment. Returns how many listings, and
/// whether anything was altered.
fn apply_segment(raw: &[u8], tree: &mut Tree) -> (usize, bool) {
    let mut altered = false;
    let got = records(&mut &raw[..], &mut |mut rec| {
        altered |= tree.apply(rec.rel, rec.pass, rec.listed_at, &rec.subdirs, &mut rec.files).1;
    });
    (got.map_or(0, |g| g.0 as usize), altered)
}

/// Whether a file failed because of what is in it (cut short, not gzip, a
/// broken record, a header that does not fit) rather than because it could
/// not be read: the share being away must not make a PC skip a good file.
fn is_damage(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{InvalidData, InvalidInput, NotFound, UnexpectedEof};
    // Gone between listing the folder and opening it (a newer full catalog replaced it): nothing to wait for.
    e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()).is_none_or(|io| matches!(io.kind(), InvalidData | InvalidInput | UnexpectedEof | NotFound))
}

type Gz = GzEncoder<BufWriter<std::fs::File>>;

fn create(path: &Path, head: &Header) -> Result<Gz> {
    let mut gz = GzEncoder::new(BufWriter::with_capacity(1 << 16, std::fs::File::create(path)?), Compression::new(3));
    serde_json::to_writer(&mut gz, head)?;
    gz.write_all(b"\n")?;
    Ok(gz)
}

fn put_record(gz: &mut Gz, body: &[u8]) -> std::io::Result<()> {
    let mut len = Vec::with_capacity(10);
    put_varint(&mut len, body.len() as u64);
    gz.write_all(&len)?;
    gz.write_all(body)
}

/// Everything written and on disk.
fn finish(gz: Gz) -> Result<()> {
    gz.finish()?.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(())
}

/// Segments this object has applied since the full catalog it started
/// from, so one that shows up late (two PCs published the same number) is
/// still applied, and none twice.
struct Seen {
    base: Mark,
    names: HashSet<String>,
}

/// The catalog files of one root (`<store>\catalog\<rootkey>`).
pub struct RootFiles {
    dir: PathBuf,
    root: String,
    seen: Mutex<Option<Seen>>,
}

impl RootFiles {
    pub fn new(dir: impl Into<PathBuf>, root: &str) -> RootFiles {
        RootFiles { dir: dir.into(), root: norm_dir(root), seen: Mutex::new(None) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    /// The catalog files present, oldest first. A folder that does not
    /// exist yet holds none; any other failure (the share is away) is an error.
    fn list(&self) -> Result<Vec<Entry>> {
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("listing {}", self.dir.display())),
        };
        let mut out: Vec<Entry> = rd.filter_map(|e| parse_name(&e.ok()?.file_name().to_string_lossy())).collect();
        out.sort_by(|a, b| (a.mark, &a.pc).cmp(&(b.mark, &b.pc)));
        Ok(out)
    }

    /// Open a catalog file and read its header.
    fn open(&self, name: &str) -> Result<(Header, impl BufRead)> {
        let f = std::fs::File::open(self.dir.join(name))?;
        let mut r = BufReader::with_capacity(1 << 16, GzDecoder::new(BufReader::with_capacity(1 << 16, f)));
        let mut line = Vec::new();
        r.by_ref().take(1 << 16).read_until(b'\n', &mut line)?;
        let head: Header = serde_json::from_slice(&line).context("no header")?;
        ensure!(head.format == FORMAT, "format {} is not known to this version", head.format);
        ensure!(lower(&norm_dir(&head.root)) == lower(&self.root), "it is the catalog of {}", head.root);
        Ok((head, r))
    }

    /// A whole root from a full catalog. Nothing of it is used unless all
    /// of it is good.
    fn read_full(&self, name: &str) -> Result<(Tree, usize)> {
        let (head, mut r) = self.open(name)?;
        let mut tree = Tree::new(&self.root);
        let got = records(&mut r, &mut |mut rec| {
            tree.apply(rec.rel, rec.pass, rec.listed_at, &rec.subdirs, &mut rec.files);
        })?;
        check(&head, got)?;
        Ok((tree, got.0 as usize))
    }

    /// The records of one segment, read and checked: all of it is good, or
    /// it is an error. Nothing is applied here, so no lock is held while
    /// the share is read.
    fn read_segment(&self, name: &str) -> Result<Vec<u8>> {
        let (head, mut r) = self.open(name)?;
        let mut raw = Vec::new();
        r.read_to_end(&mut raw)?;
        check(&head, records(&mut &raw[..], &mut |_| {})?)?;
        Ok(raw)
    }

    /// What to do about a file that could not be used. A damaged one (or
    /// one of a format this version does not know) is passed over for
    /// good: `Ok`. When the share itself failed, nothing is skipped and the
    /// caller hears of it, to try again later: `Err`.
    fn pass_over(&self, name: &str, e: anyhow::Error) -> Result<()> {
        if is_damage(&e) {
            tracing::warn!("catalog file {} skipped: {e:#}", self.dir.join(name).display());
            return Ok(());
        }
        Err(e.context(format!("reading {}", self.dir.join(name).display())))
    }

    /// The newest readable full catalog among `fulls` with every later
    /// segment applied, put into `cat` in one step. `None` when no full
    /// catalog could be used.
    fn reload(&self, cat: &Catalog, entries: &[Entry], fulls: &[&Entry]) -> Result<Option<(Mark, usize)>> {
        for full in fulls.iter().rev() {
            let (mut tree, mut n) = match self.read_full(&full.name) {
                Ok(x) => x,
                Err(e) => {
                    self.pass_over(&full.name, e)?;
                    continue;
                }
            };
            let mut seen = Seen { base: full.mark, names: HashSet::new() };
            let mut mark = full.mark;
            for seg in entries.iter().filter(|e| !e.full && e.mark > full.mark) {
                match self.read_segment(&seg.name) {
                    Ok(raw) => n += apply_segment(&raw, &mut tree).0,
                    Err(e) => self.pass_over(&seg.name, e)?,
                }
                seen.names.insert(seg.name.clone());
                mark = mark.max(seg.mark);
            }
            tree.shrink_to_fit();
            cat.swap_root(tree);
            *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(seen);
            return Ok(Some((mark, n)));
        }
        Ok(None)
    }

    /// Apply to the root in `cat` the segments not applied yet that come
    /// after `mark` (or that showed up late). Moves `mark` on, also when
    /// the share fails part-way: what was applied stays applied.
    fn catch_up(&self, cat: &Catalog, entries: &[Entry], mark: &mut Mark) -> Result<usize> {
        let mut guard = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let seen = guard.get_or_insert_with(|| Seen { base: *mark, names: HashSet::new() });
        let mut n = 0;
        let from = *mark;
        for seg in entries.iter().filter(|e| !e.full) {
            if !(seg.mark > from || seg.mark > seen.base) {
                continue;
            }
            if seen.names.contains(&seg.name) {
                *mark = (*mark).max(seg.mark);
                continue;
            }
            match self.read_segment(&seg.name) {
                Ok(raw) => match cat.with_tree(&self.root, |tree| apply_segment(&raw, tree)) {
                    Some(k) => n += k,
                    None => return Ok(n),
                },
                Err(e) => self.pass_over(&seg.name, e)?,
            }
            // A damaged file stays damaged: it is not tried again.
            seen.names.insert(seg.name.clone());
            *mark = (*mark).max(seg.mark);
        }
        Ok(n)
    }

    /// Load the newest full catalog and every later segment into `cat`
    /// (the root is added). Returns the mark reached; `Mark{0,0}` when empty.
    /// Damaged files are passed over with a warning; an error means the
    /// share could not be read.
    pub fn load(&self, cat: &Catalog) -> Result<Mark> {
        cat.add_root(&self.root);
        let entries = self.list()?;
        let fulls: Vec<&Entry> = entries.iter().filter(|e| e.full).collect();
        if let Some((mark, _)) = self.reload(cat, &entries, &fulls)? {
            return Ok(mark);
        }
        let mut mark = Mark::default();
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.catch_up(cat, &entries, &mut mark)?;
        Ok(mark)
    }

    /// Apply what other PCs published since `mark`. Returns listings applied.
    pub fn follow(&self, cat: &Catalog, mark: &mut Mark) -> Result<usize> {
        cat.add_root(&self.root);
        let entries = self.list()?;
        let fulls: Vec<&Entry> = entries.iter().filter(|e| e.full && e.mark > *mark).collect();
        if let Some((reached, n)) = self.reload(cat, &entries, &fulls)? {
            *mark = reached;
            return Ok(n);
        }
        self.catch_up(cat, &entries, mark)
    }

    /// Start writing segments for `pass`, continuing after the last one.
    pub fn writer(&self, pc: &str, pass: i64) -> Result<SegmentWriter> {
        let next = self.last_seq(pass)? + 1;
        Ok(SegmentWriter { dir: self.dir.clone(), root: self.root.clone(), root_lower: lower(&self.root), pc: safe_name(pc), pass, next, records: Vec::new(), body: Vec::new(), dirs: 0, files: 0 })
    }

    fn last_seq(&self, pass: i64) -> Result<u32> {
        Ok(self.list()?.iter().filter(|e| e.mark.pass == pass).map(|e| e.mark.seq).max().unwrap_or(0))
    }

    /// Write the whole root from memory as a full catalog at `mark` and
    /// delete the files it replaces (older than one hour).
    pub fn write_full(&self, cat: &Catalog, mark: Mark) -> Result<()> {
        let snap = cat.snapshot_root(&self.root).ok_or_else(|| anyhow!("{} is not in the catalog", self.root))?;
        let listed = || snap.dirs.iter().filter(|d| d.listed);
        let head = Header { format: FORMAT, root: self.root.clone(), pass: mark.pass, seq: mark.seq, created: now(), by: this_pc(), dirs: listed().count() as u64, files: listed().map(|d| d.nfiles as u64).sum() };
        std::fs::create_dir_all(&self.dir)?;
        let name = format!("full-{}-{:08}.cat", mark.pass, mark.seq);
        let (dest, tmp) = (self.dir.join(&name), self.dir.join(format!("{name}.{}.{}.tmp", safe_name(&this_pc()), std::process::id())));
        let written = (|| -> Result<()> {
            let mut gz = create(&tmp, &head)?;
            let mut body = Vec::new();
            let mut failed = None;
            let top = self.root.len();
            snap.walk(&mut |i, path| {
                let d = &snap.dirs[i];
                if !d.listed {
                    return true;
                }
                begin_record(&mut body, path[top..].trim_start_matches('\\'), d.pass, d.listed_at);
                put_varint(&mut body, snap.children(i).count() as u64);
                snap.children(i).for_each(|c| put_str(&mut body, snap.name(c)));
                put_varint(&mut body, d.nfiles as u64);
                if let Some(blob) = d.files.as_deref() {
                    let mut it = Entries::new(blob);
                    while let Some((size, mtime)) = it.next() {
                        put_file(&mut body, &it.name, size, mtime);
                    }
                }
                failed = put_record(&mut gz, &body).err();
                failed.is_none()
            });
            if let Some(e) = failed {
                return Err(e.into());
            }
            finish(gz)?;
            retry(|| std::fs::rename(&tmp, &dest))?;
            Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written.with_context(|| format!("writing {}", dest.display()))?;
        self.sweep(mark, &dest);
        Ok(())
    }

    /// Delete what the full catalog at `mark` replaces, and temporary files
    /// a crash left, once older than [`KEEP`]. Age is measured against the
    /// new file's own time, so only the file server's clock is involved.
    fn sweep(&self, mark: Mark, full: &Path) {
        let Ok(new) = std::fs::metadata(full).and_then(|m| m.modified()) else { return };
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let replaced = match parse_name(&name) {
                Some(e) if e.full => e.mark < mark,
                Some(e) => e.mark <= mark,
                None => name.ends_with(".tmp"),
            };
            let old = ent.metadata().and_then(|m| m.modified()).ok().and_then(|t| new.duration_since(t).ok()).is_some_and(|age| age > KEEP);
            if replaced && old {
                let _ = std::fs::remove_file(ent.path());
            }
        }
    }

    pub fn state(&self) -> Option<RootState> {
        serde_json::from_slice(&std::fs::read(self.dir.join("state.json")).ok()?).ok()
    }

    pub fn set_state(&self, s: &RootState) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("state.json.{}.{}.tmp", safe_name(&this_pc()), std::process::id()));
        replace(&tmp, &self.dir.join("state.json"), &serde_json::to_vec_pretty(s)?).with_context(|| format!("writing {}", self.dir.join("state.json").display()))
    }
}

/// Collects listings and publishes them as numbered segments.
pub struct SegmentWriter {
    dir: PathBuf,
    root: String,
    root_lower: String,
    pc: String,
    pass: i64,
    next: u32,
    /// The records added, each with its length, ready to be written.
    records: Vec<u8>,
    body: Vec<u8>,
    dirs: u64,
    files: u64,
}

impl SegmentWriter {
    /// A listing outside the root is dropped.
    pub fn add(&mut self, l: &Listing) {
        let dir = norm_dir(&l.dir);
        let Some(rel) = rel_of(&self.root_lower, &dir) else {
            tracing::warn!("{} is not under {}: not published", l.dir, self.root);
            return;
        };
        begin_record(&mut self.body, rel.trim_start_matches('\\'), l.pass, l.listed_at);
        put_varint(&mut self.body, l.subdirs.len() as u64);
        l.subdirs.iter().for_each(|s| put_str(&mut self.body, s));
        put_varint(&mut self.body, l.files.len() as u64);
        l.files.iter().for_each(|(name, size, mtime)| put_file(&mut self.body, name, *size, *mtime));
        put_varint(&mut self.records, self.body.len() as u64);
        self.records.extend_from_slice(&self.body);
        self.dirs += 1;
        self.files += l.files.len() as u64;
    }

    /// Listings added and not yet written.
    pub fn pending(&self) -> usize {
        self.dirs as usize
    }

    /// Write what was added as one segment file (temp + rename). Returns
    /// its mark; `None` when there was nothing. On an error the listings
    /// stay pending.
    pub fn flush(&mut self) -> Result<Option<Mark>> {
        if self.dirs == 0 {
            return Ok(None);
        }
        std::fs::create_dir_all(&self.dir)?;
        // Another PC may have published since (it took the walk over): never reuse a number.
        let taken = RootFiles::new(&self.dir, &self.root).last_seq(self.pass)?;
        let seq = self.next.max(taken + 1);
        let head = Header { format: FORMAT, root: self.root.clone(), pass: self.pass, seq, created: now(), by: self.pc.clone(), dirs: self.dirs, files: self.files };
        let name = format!("seg-{}-{:08}-{}.cat", self.pass, seq, self.pc);
        let (dest, tmp) = (self.dir.join(&name), self.dir.join(format!("{name}.{}.tmp", std::process::id())));
        let written = (|| -> Result<()> {
            let mut gz = create(&tmp, &head)?;
            gz.write_all(&self.records)?;
            finish(gz)?;
            retry(|| std::fs::rename(&tmp, &dest))?;
            Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written.with_context(|| format!("writing {}", dest.display()))?;
        self.next = seq + 1;
        self.records.clear();
        (self.dirs, self.files) = (0, 0);
        Ok(Some(Mark { pass: self.pass, seq }))
    }
}

#[cfg(test)]
mod tests;
