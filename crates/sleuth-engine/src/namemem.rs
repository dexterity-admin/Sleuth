//! Every file name in memory, so a name search scans them all at once the
//! way Everything does, instead of reading the index from disk (on a hard
//! disk a short or common query took a minute that way).
//!
//! Loaded from the index in the background when the worker starts; until
//! then name searches use the index as before. Every file the index adds,
//! changes or forgets is applied here too (see `Store::upsert_file` and
//! `Store::delete_file`), so the two never drift apart.

use crate::store::{Scope, Store, state};
use anyhow::Result;
use sleuth_core::analyzer::{Term, glob_match};
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// `(id, name, path)`, as the index's own name search returns.
pub type NameRow = (i64, String, String);

/// A file as the search results need it.
pub struct MemFile {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Default)]
pub struct NameMem {
    inner: RwLock<Inner>,
    ready: AtomicBool,
}

#[derive(Default)]
struct Inner {
    /// Sorted file ids, with their records at the same position.
    ids: Vec<i64>,
    recs: Vec<Rec>,
    /// All names, one after another.
    arena: String,
    dirs: Vec<Box<str>>,
    dir_ix: HashMap<Box<str>, u32>,
    /// Changes made while the names were still loading.
    pending: Vec<Op>,
}

#[derive(Clone, Copy)]
struct Rec {
    off: u32,
    len: u32,
    /// `GONE_DIR` once the file is forgotten.
    dir: u32,
    size: u64,
    mtime: i64,
}

const GONE_DIR: u32 = u32::MAX;

enum Op {
    Put { id: i64, dir: String, name: String, size: u64, mtime: i64 },
    Remove(i64),
}

impl Inner {
    fn dir_id(&mut self, dir: &str) -> u32 {
        if let Some(&i) = self.dir_ix.get(dir) {
            return i;
        }
        let i = self.dirs.len() as u32;
        self.dirs.push(dir.into());
        self.dir_ix.insert(dir.into(), i);
        i
    }

    fn put(&mut self, id: i64, dir: &str, name: &str, size: u64, mtime: i64) {
        let d = self.dir_id(dir);
        let off = self.arena.len() as u32;
        self.arena.push_str(name);
        let rec = Rec { off, len: name.len() as u32, dir: d, size, mtime };
        match self.ids.binary_search(&id) {
            Ok(i) => self.recs[i] = rec,
            Err(i) => {
                self.ids.insert(i, id);
                self.recs.insert(i, rec);
            }
        }
    }

    fn remove(&mut self, id: i64) {
        if let Ok(i) = self.ids.binary_search(&id) {
            self.recs[i].dir = GONE_DIR;
        }
    }

    fn apply(&mut self, op: Op) {
        match op {
            Op::Put { id, dir, name, size, mtime } => self.put(id, &dir, &name, size, mtime),
            Op::Remove(id) => self.remove(id),
        }
    }

    fn name(&self, r: &Rec) -> &str {
        &self.arena[r.off as usize..(r.off + r.len) as usize]
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') { format!("{dir}{name}") } else { format!("{dir}\\{name}") }
}

/// `text` in lower case, written into `buf` (no allocation for ASCII).
fn lower_into<'a>(text: &str, buf: &'a mut String) -> &'a str {
    buf.clear();
    if text.is_ascii() {
        buf.extend(text.bytes().map(|b| b.to_ascii_lowercase() as char));
    } else {
        buf.extend(text.chars().flat_map(char::to_lowercase));
    }
    buf
}

/// Letters and digits only, lower case (as `analyzer::sep_form`).
fn sep_into<'a>(lower: &str, buf: &'a mut String) -> &'a str {
    buf.clear();
    buf.extend(lower.chars().filter(|c| c.is_alphanumeric()));
    buf
}

/// What one name scan looks for.
struct Want<'t> {
    term: &'t Term,
    pattern: Option<String>,
    literal_only: bool,
}

impl<'t> Want<'t> {
    fn new(term: &'t Term) -> Want<'t> {
        Want { term, pattern: term.has_wildcards().then(|| format!("*{}*", term.raw)), literal_only: term.is_literal_only() || term.is_punctuation() }
    }

    /// 0 the whole name, 1 the text as typed, 2 with other separators or a
    /// pattern; `None` when `explain_name` could not match this name.
    fn group(&self, lower: &str, sep_buf: &mut String) -> Option<u8> {
        if let Some(p) = &self.pattern {
            return glob_match(p, lower).then_some(2);
        }
        let raw = self.term.raw.as_str();
        if lower.contains(raw) {
            let whole = lower == raw || lower.rsplit_once('.').is_some_and(|(stem, _)| stem == raw);
            return Some(if whole { 0 } else { 1 });
        }
        if self.literal_only || self.term.sep.is_empty() {
            return None;
        }
        sep_into(lower, sep_buf).contains(self.term.sep.as_str()).then_some(2)
    }

    /// For words that may also be in the folder path.
    fn in_text(&self, lower: &str, sep_buf: &mut String) -> bool {
        self.group(lower, sep_buf).is_some()
    }
}

impl NameMem {
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Number of files held (forgotten ones excluded).
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().recs.iter().filter(|r| r.dir != GONE_DIR).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read every name from the index. Changes made meanwhile are applied
    /// after, so nothing added or removed during the load is missed.
    pub fn load(&self, store: &Store) -> Result<usize> {
        let mut fresh = Inner::default();
        {
            let mut st = store.conn.prepare("SELECT id, dir, name, size, mtime FROM files WHERE read_state<>?1 ORDER BY id")?;
            let mut rows = st.query([state::GONE])?;
            while let Some(r) = rows.next()? {
                let id: i64 = r.get(0)?;
                let dir: String = r.get(1)?;
                let name: String = r.get(2)?;
                let size = r.get::<_, i64>(3)? as u64;
                let mtime: i64 = r.get(4)?;
                let d = fresh.dir_id(&dir);
                let off = fresh.arena.len() as u32;
                fresh.arena.push_str(&name);
                fresh.ids.push(id);
                fresh.recs.push(Rec { off, len: name.len() as u32, dir: d, size, mtime });
            }
        }
        let n = fresh.ids.len();
        let mut g = self.inner.write().unwrap();
        for op in std::mem::take(&mut g.pending) {
            fresh.apply(op);
        }
        *g = fresh;
        self.ready.store(true, Ordering::Release);
        Ok(n)
    }

    pub fn put(&self, id: i64, dir: &str, name: &str, size: u64, mtime: i64) {
        let mut g = self.inner.write().unwrap();
        if self.is_ready() {
            g.put(id, dir, name, size, mtime);
        } else {
            g.pending.push(Op::Put { id, dir: dir.into(), name: name.into(), size, mtime });
        }
    }

    pub fn remove(&self, id: i64) {
        let mut g = self.inner.write().unwrap();
        if self.is_ready() {
            g.remove(id);
        } else {
            g.pending.push(Op::Remove(id));
        }
    }

    pub fn get(&self, id: i64) -> Option<MemFile> {
        if !self.is_ready() {
            return None;
        }
        let g = self.inner.read().unwrap();
        let i = g.ids.binary_search(&id).ok()?;
        let r = g.recs[i];
        if r.dir == GONE_DIR {
            return None;
        }
        let name = g.name(&r).to_string();
        Some(MemFile { path: join(&g.dirs[r.dir as usize], &name), name, size: r.size, mtime: r.mtime })
    }

    /// Whether this scope can be answered from memory: folder and extension
    /// limits can, drawing-detail filters (`rev:`, `dwg:`) need the index.
    pub fn serves(&self, scope: &Scope) -> bool {
        self.is_ready() && scope.rev.is_none() && scope.dwg.is_none() && scope.disc.is_none()
    }

    /// Files whose name may match `term` (a name term, not a path), whole
    /// names first, then the text as typed, then the rest; newer first
    /// within each. At most `cap`.
    pub fn candidates(&self, term: &Term, scope: &Scope, cap: usize) -> Vec<NameRow> {
        let want = Want::new(term);
        self.scan(scope, cap, |name_lower, _dir_lower, sep| want.group(name_lower, sep))
    }

    /// Files where every term is in the name or the folder path.
    pub fn candidates_all(&self, terms: &[Term], scope: &Scope, cap: usize) -> Vec<NameRow> {
        let wants: Vec<Want> = terms.iter().map(Want::new).collect();
        self.scan(scope, cap, |name_lower, dir_lower, sep| {
            let mut best = 2u8;
            for w in &wants {
                match w.group(name_lower, sep) {
                    Some(g) => best = best.min(g.max(1)),
                    None if w.in_text(dir_lower, sep) => {}
                    None => return None,
                }
            }
            Some(best)
        })
    }

    fn scan(&self, scope: &Scope, cap: usize, test: impl Fn(&str, &str, &mut String) -> Option<u8> + Sync) -> Vec<NameRow> {
        let g = self.inner.read().unwrap();
        // Which folders are inside the scope, worked out once per folder.
        let roots: Vec<String> = scope.roots.iter().map(|r| r.trim_end_matches('\\').to_lowercase()).collect();
        let dir_lower: Vec<String> = g.dirs.iter().map(|d| d.to_lowercase()).collect();
        let dir_ok: Vec<bool> = dir_lower
            .iter()
            .map(|d| roots.is_empty() || roots.iter().any(|r| d == r || d.starts_with(&format!("{r}\\")) || (r.len() == 2 && d.starts_with(r.as_str()))))
            .collect();
        let ext = scope.ext.as_ref().map(|e| format!(".{}", e.trim_start_matches('.').to_lowercase()));
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16);
        let chunk = g.recs.len().div_ceil(threads).max(1);
        let mut found: Vec<(u8, i64, u32)> = std::thread::scope(|s| {
            let parts: Vec<_> = g
                .recs
                .chunks(chunk)
                .enumerate()
                .map(|(k, recs)| {
                    let (g, test, dir_ok, dir_lower, ext) = (&*g, &test, &dir_ok, &dir_lower, &ext);
                    s.spawn(move || {
                        let (mut lower, mut sep) = (String::new(), String::new());
                        let mut out = Vec::new();
                        for (j, r) in recs.iter().enumerate() {
                            if r.dir == GONE_DIR || !dir_ok[r.dir as usize] {
                                continue;
                            }
                            let name = lower_into(g.name(r), &mut lower);
                            if ext.as_ref().is_some_and(|e| !name.ends_with(e.as_str())) {
                                continue;
                            }
                            if let Some(grp) = test(name, &dir_lower[r.dir as usize], &mut sep) {
                                out.push((grp, r.mtime, (k * chunk + j) as u32));
                            }
                        }
                        out
                    })
                })
                .collect();
            parts.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        let key = |f: &(u8, i64, u32)| (f.0, std::cmp::Reverse(f.1));
        if found.len() > cap && cap > 0 {
            found.select_nth_unstable_by_key(cap - 1, key);
            found.truncate(cap);
        }
        found.sort_by_key(key);
        found
            .into_iter()
            .map(|(_, _, i)| {
                let r = &g.recs[i as usize];
                let name = g.name(r).to_string();
                (g.ids[i as usize], name.clone(), join(&g.dirs[r.dir as usize], &name))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(files: &[(i64, &str, &str)]) -> NameMem {
        let m = NameMem::default();
        m.ready.store(true, Ordering::Release);
        for (id, dir, name) in files {
            m.put(*id, dir, name, 1, *id);
        }
        m
    }

    fn names(rows: &[NameRow]) -> Vec<&str> {
        rows.iter().map(|r| r.1.as_str()).collect()
    }

    #[test]
    fn finds_names_like_everything() {
        let m = mem(&[
            (1, r"\\srv\p\Rev-1", "14-01-15-2632_PSV-2809-01_HOLD.pdf"),
            (2, r"\\srv\p", "FSK_Batch-1.zip"),
            (3, r"\\srv\p", "fsk.xlsx"),
            (4, r"C:\", "Обычный.pmvx"),
            (5, r"\\srv\other", "PSV2809.dwg"),
            (6, r"\\srv\p", "plan & notes.txt"),
        ]);
        let all = Scope::default();
        let t = |q: &str| Term::new(q).unwrap();
        assert_eq!(names(&m.candidates(&t("fsk"), &all, 10)), vec!["fsk.xlsx", "FSK_Batch-1.zip"], "whole name first");
        assert_eq!(names(&m.candidates(&t("psv-2809"), &all, 10)), vec!["14-01-15-2632_PSV-2809-01_HOLD.pdf", "PSV2809.dwg"]);
        assert_eq!(names(&m.candidates(&t("обычный"), &all, 10)), vec!["Обычный.pmvx"]);
        assert_eq!(names(&m.candidates(&t("*.zip"), &all, 10)), vec!["FSK_Batch-1.zip"]);
        assert_eq!(names(&m.candidates(&t("&"), &all, 10)), vec!["plan & notes.txt"]);
        assert_eq!(names(&m.candidates(&t("x"), &Scope { ext: Some("dwg".into()), ..Default::default() }, 10)), Vec::<&str>::new());
        assert_eq!(names(&m.candidates(&t("psv"), &Scope::roots(&[r"\\srv\other".into()]), 10)), vec!["PSV2809.dwg"]);
        // Every word in the name or the folder path.
        assert_eq!(names(&m.candidates_all(&[t("rev-1"), t("psv")], &all, 10)), vec!["14-01-15-2632_PSV-2809-01_HOLD.pdf"]);
        let row = m.get(2).unwrap();
        assert_eq!(row.path, r"\\srv\p\FSK_Batch-1.zip");
        m.remove(2);
        assert!(m.get(2).is_none());
        assert_eq!(names(&m.candidates(&t("fsk"), &all, 10)), vec!["fsk.xlsx"]);
        // A rename keeps one entry.
        m.put(3, r"\\srv\p", "fsk-old.xlsx", 1, 3);
        assert_eq!(names(&m.candidates(&t("fsk"), &all, 10)), vec!["fsk-old.xlsx"]);
        assert_eq!(m.get(4).unwrap().path, r"C:\Обычный.pmvx");
    }

    #[test]
    fn changes_during_the_load_are_kept() {
        let s = Store::open_in_memory().unwrap();
        let (a, _) = s.upsert_file(r"C:\a\one.pdf", r"C:\a", "one.pdf", 1, 1, 1, state::UNREAD).unwrap();
        let (b, _) = s.upsert_file(r"C:\a\two.pdf", r"C:\a", "two.pdf", 1, 1, 1, state::UNREAD).unwrap();
        let m = NameMem::default();
        m.put(99, r"C:\a", "three.pdf", 1, 1);
        m.remove(b);
        assert_eq!(m.load(&s).unwrap(), 2);
        assert!(m.get(a).is_some() && m.get(b).is_none() && m.get(99).is_some());
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn a_million_names_scan_quickly() {
        let m = NameMem::default();
        m.ready.store(true, Ordering::Release);
        for i in 0..1_000_000i64 {
            m.put(i, &format!(r"\\srv\p\{}", i % 5000), &format!("ISO-{i:07}-01_rev{}.pdf", i % 7), 1, i);
        }
        let t0 = std::time::Instant::now();
        let r = m.candidates(&Term::new("x").unwrap(), &Scope::default(), 8000);
        let r2 = m.candidates(&Term::new("0012345").unwrap(), &Scope::default(), 8000);
        let ms = t0.elapsed().as_millis();
        assert!(r.is_empty() && r2.len() == 1);
        assert!(ms < 3000, "two scans of a million names took {ms} ms");
    }
}
