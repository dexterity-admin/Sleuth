//! Every file name of every share in memory, as folder listings.
//!
//! A PC keeps no index of its own: it follows the listings the walking PC
//! publishes (see [`crate::files`]) and applies each one here. A listing
//! replaces one folder's contents, so the catalog never needs file ids, and
//! a folder that disappears takes everything below it along.
//!
//! Layout, chosen for 7.5 million files: per root a table of folders (56
//! bytes each) and, per folder, one block holding its files sorted by name:
//! each name as the part that differs from the one before, then size and
//! modified time as variable-length numbers. A name search decodes the
//! blocks on all cores; nothing but the hits is ever allocated. Each block
//! starts with a small bit set of the letter pairs its names hold, so a
//! search passes most folders without reading a name.

use sleuth_core::analyzer::{Term, glob_match, sep_form};
use sleuth_core::kinds::stem_key;
use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::wire::{get_varint, put_str, put_varint, unzigzag, zigzag};

pub use crate::files::{Mark, RootFiles, RootState, SegmentWriter};

/// A file as the search results need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
}

/// One folder as listed. `dir` is the full canonical folder path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub dir: String,
    /// Walk pass that listed it (resumption); 0 for a one-off relist.
    pub pass: i64,
    pub listed_at: i64,
    /// Names.
    pub subdirs: Vec<String>,
    /// Name, size, modified time.
    pub files: Vec<(String, u64, i64)>,
}

/// What one listing altered. Files and folders below a removed subfolder
/// are counted too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Change {
    pub files_added: u32,
    pub files_changed: u32,
    pub files_removed: u32,
    pub dirs_removed: u32,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        *self == Change::default()
    }
}

/// Where a name search looks: under any of `roots` (everywhere when empty),
/// and only files with extension `ext` when given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameScope {
    pub roots: Vec<String>,
    pub ext: Option<String>,
}

const NONE: u32 = u32::MAX;
/// `Dir::parent` of a slot waiting to be reused.
const DEAD: u32 = u32::MAX - 1;
/// Every this many files a name is stored whole, so one file can be read
/// back without decoding the folder from its start.
const RESTART: usize = 16;

struct Dir {
    parent: u32,
    /// Next folder with the same parent.
    next: u32,
    /// First subfolder.
    child: u32,
    name_off: u32,
    name_len: u16,
    /// Its contents are held (a subfolder only named by its parent's
    /// listing is known but not listed).
    listed: bool,
    nfiles: u32,
    pass: i64,
    listed_at: i64,
    /// The files, see [`Entries`]. Shared so that a snapshot costs nothing.
    files: Option<Arc<[u8]>>,
}

/// The files of one folder, decoded in name order. Each entry is: bytes
/// shared with the previous name (one byte), the rest of the name
/// (length-prefixed), size, modified time.
pub(crate) struct Entries<'a> {
    blob: &'a [u8],
    pos: usize,
    /// Start of the last entry that needs no earlier one.
    restart: u32,
    /// Entries read since.
    k: u8,
    pub name: String,
}

impl<'a> Entries<'a> {
    pub(crate) fn new(blob: &'a [u8]) -> Entries<'a> {
        let start = Grams::of(blob).1;
        Entries { blob, pos: start, restart: start as u32, k: 0, name: String::new() }
    }

    fn at(blob: &'a [u8], restart: u32) -> Entries<'a> {
        Entries { blob, pos: restart as usize, restart, k: 0, name: String::new() }
    }

    /// Size and modified time of the next file; its name is in `self.name`.
    pub(crate) fn next(&mut self) -> Option<(u64, i64)> {
        let start = self.pos;
        let shared = *self.blob.get(self.pos)? as usize;
        self.pos += 1;
        let n = get_varint(self.blob, &mut self.pos)? as usize;
        let rest = std::str::from_utf8(self.blob.get(self.pos..self.pos.checked_add(n)?)?).ok()?;
        self.pos += n;
        if shared == 0 {
            self.restart = start as u32;
            self.k = 0;
        } else {
            self.k = self.k.wrapping_add(1);
        }
        if !self.name.is_char_boundary(shared) {
            return None;
        }
        self.name.truncate(shared);
        self.name.push_str(rest);
        let size = get_varint(self.blob, &mut self.pos)?;
        let mtime = unzigzag(get_varint(self.blob, &mut self.pos)?);
        Some((size, mtime))
    }
}

fn shared_prefix(a: &str, b: &str) -> usize {
    let mut n = a.bytes().zip(b.bytes()).take(255).take_while(|(x, y)| x == y).count();
    while !b.is_char_boundary(n) {
        n -= 1;
    }
    n
}

/// Which pairs of neighbouring letters and digits occur in the file names
/// of a folder (separators dropped, lower case), one bit per pair, at the
/// front of the folder's block. Whatever way a name matches a term, it holds
/// the term's pairs, so most folders are passed without reading one name.
struct Grams<'a> {
    bits: &'a [u8],
    shift: u32,
}

/// Bits per file: about a third are set in a folder of related drawings.
const GRAM_BITS: usize = 20;

fn gram(a: u8, b: u8) -> u32 {
    (((a as u32) << 8) | b as u32).wrapping_mul(0x9e37_79b1)
}

/// The pairs of `sep` (letters and digits, lower case), for a term.
fn push_grams(sep: &[u8], out: &mut Vec<u32>) {
    out.extend(sep.windows(2).map(|w| gram(w[0], w[1])));
}

impl<'a> Grams<'a> {
    /// The pair bits of a block, and where its entries start. The first
    /// byte says how many bits: 64 shifted left by it.
    fn of(blob: &'a [u8]) -> (Grams<'a>, usize) {
        let k = blob.first().copied().unwrap_or(0).min(7) as u32;
        let end = (1 + (8usize << k)).min(blob.len());
        (Grams { bits: &blob[end.min(1)..end], shift: 32 - (6 + k) }, end)
    }

    fn has_all(&self, grams: &[u32]) -> bool {
        grams.iter().all(|g| {
            let bit = (g >> self.shift) as usize;
            self.bits.get(bit >> 3).is_none_or(|b| b & (1 << (bit & 7)) != 0)
        })
    }
}

/// `files` must be sorted by name.
fn encode(files: &[(&str, u64, i64)], out: &mut Vec<u8>, bufs: &mut NameBufs) -> Option<Arc<[u8]>> {
    if files.is_empty() {
        return None;
    }
    let k = ((files.len().min(1024) * GRAM_BITS).next_power_of_two().clamp(64, 8192) / 64).trailing_zeros();
    let shift = 32 - (6 + k);
    out.clear();
    out.push(k as u8);
    out.resize(1 + (8usize << k), 0);
    let mut prev = "";
    for (i, (name, size, mtime)) in files.iter().enumerate() {
        for w in Forms::of(name, false, bufs).sep().windows(2) {
            let bit = (gram(w[0], w[1]) >> shift) as usize;
            out[1 + (bit >> 3)] |= 1 << (bit & 7);
        }
        let shared = if i % RESTART == 0 { 0 } else { shared_prefix(prev, name) };
        out.push(shared as u8);
        put_str(out, &name[shared..]);
        put_varint(out, *size);
        put_varint(out, zigzag(*mtime));
        prev = name;
    }
    Some(Arc::from(&out[..]))
}

/// The map from (parent, lower-case name) to folder is keyed by a hash we
/// compute ourselves; hashing it again would be wasted work.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | b as u64;
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}

fn child_key(parent: u32, name: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ (parent as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut eat = |b: u8| h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    if name.is_ascii() {
        name.bytes().for_each(|b| eat(b.to_ascii_lowercase()));
    } else {
        let mut buf = [0u8; 4];
        for c in name.chars().flat_map(char::to_lowercase) {
            c.encode_utf8(&mut buf).bytes().for_each(&mut eat);
        }
    }
    // Spread the bits: the map uses the low ones for the slot and the high ones as a tag.
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^ (h >> 33)
}

fn eq_ci(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() { a.eq_ignore_ascii_case(b) } else { a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase)) }
}

fn push_lower(text: &str, out: &mut String) {
    if text.is_ascii() {
        let from = out.len();
        out.push_str(text);
        out[from..].make_ascii_lowercase();
    } else {
        out.extend(text.chars().flat_map(char::to_lowercase));
    }
}

/// `text` in lower case, written into `buf` (no allocation for ASCII).
fn lower_into<'a>(text: &str, buf: &'a mut String) -> &'a str {
    buf.clear();
    push_lower(text, buf);
    buf
}

pub(crate) fn lower(text: &str) -> String {
    let mut s = String::with_capacity(text.len());
    push_lower(text, &mut s);
    s
}

/// Letters and digits only, lower case (as `analyzer::sep_form`).
fn sep_into<'a>(lower: &str, buf: &'a mut Vec<u8>) -> &'a [u8] {
    buf.clear();
    if lower.is_ascii() {
        buf.extend(lower.bytes().filter(u8::is_ascii_alphanumeric));
    } else {
        let mut utf8 = [0u8; 4];
        for c in lower.chars().filter(|c| c.is_alphanumeric()) {
            buf.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
        }
    }
    buf
}

/// Whether `needle` is in `hay` (as `str::contains`; both are UTF-8). A
/// file name is short and there are millions: looking for the first byte
/// beats preparing a cleverer search for each name.
fn has(hay: &[u8], needle: &[u8]) -> bool {
    let Some((first, rest)) = needle.split_first() else { return true };
    if needle.len() > hay.len() {
        return false;
    }
    let starts = &hay[..=hay.len() - needle.len()];
    let mut from = 0;
    while let Some(p) = starts[from..].iter().position(|b| b == first) {
        let at = from + p;
        if hay[at + 1..at + needle.len()] == *rest {
            return true;
        }
        from = at + 1;
    }
    false
}

/// [`has`] without regard to case, for an ASCII `hay` (a name as it is
/// stored); `needle` is lower case.
fn has_ci(hay: &[u8], needle: &[u8]) -> bool {
    let Some((first, rest)) = needle.split_first() else { return true };
    if needle.len() > hay.len() {
        return false;
    }
    let starts = &hay[..=hay.len() - needle.len()];
    let mut from = 0;
    while let Some(p) = starts[from..].iter().position(|b| b.to_ascii_lowercase() == *first) {
        let at = from + p;
        if hay[at + 1..at + needle.len()].eq_ignore_ascii_case(rest) {
            return true;
        }
        from = at + 1;
    }
    false
}

/// [`glob_match`] for ASCII, where a character is a byte: no allocation.
fn glob_ascii(p: &[u8], t: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || (p[pi] != b'*' && p[pi] == t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') { format!("{dir}{name}") } else { format!("{dir}\\{name}") }
}

/// A folder path as the catalog writes it: no trailing backslash except on
/// a drive root (`C:\`).
pub fn norm_dir(p: &str) -> String {
    let t = p.trim().trim_end_matches('\\');
    if t.len() == 2 && t.ends_with(':') { format!("{t}\\") } else { t.to_string() }
}

/// What is left of `path` after `prefix_lower`, compared without case.
fn strip_prefix_ci<'p>(path: &'p str, prefix_lower: &str) -> Option<&'p str> {
    let mut want = prefix_lower.chars().peekable();
    for (i, c) in path.char_indices() {
        if want.peek().is_none() {
            return Some(&path[i..]);
        }
        for l in c.to_lowercase() {
            if want.next() != Some(l) {
                return None;
            }
        }
    }
    want.peek().is_none().then_some("")
}

/// `path` relative to the root `root_lower`, when it is that root or inside it.
pub(crate) fn rel_of<'p>(root_lower: &str, path: &'p str) -> Option<&'p str> {
    let rest = strip_prefix_ci(path, root_lower)?;
    (rest.is_empty() || rest.starts_with('\\') || root_lower.ends_with('\\')).then_some(rest)
}

/// Whether folder `d` is `r` or below it (both lower case, `r` without a
/// trailing backslash, so a drive is `c:`).
fn inside(d: &str, r: &str) -> bool {
    d == r || (d.len() > r.len() && d.starts_with(r) && d.as_bytes()[r.len()] == b'\\') || (r.len() == 2 && d.starts_with(r))
}

/// One root: its folders and their files.
pub(crate) struct Tree {
    root: String,
    root_lower: String,
    /// `[0]` is the root folder itself.
    dirs: Vec<Dir>,
    free: Vec<u32>,
    /// Folder names, one after another.
    names: String,
    names_dead: usize,
    index: HashMap<u64, u32, BuildHasherDefault<KeyHasher>>,
    /// Folders whose key was already taken by another (64-bit hashes almost
    /// never collide; when they do the folder is found here).
    clashes: Vec<u32>,
    listed: u64,
    files: u64,
    scratch: Vec<u8>,
    bufs: NameBufs,
}

impl Tree {
    /// `root` in [`norm_dir`] form.
    pub(crate) fn new(root: &str) -> Tree {
        let top = Dir { parent: NONE, next: NONE, child: NONE, name_off: 0, name_len: 0, listed: false, nfiles: 0, pass: 0, listed_at: 0, files: None };
        Tree { root: root.to_string(), root_lower: lower(root), dirs: vec![top], free: Vec::new(), names: String::new(), names_dead: 0, index: HashMap::default(), clashes: Vec::new(), listed: 0, files: 0, scratch: Vec::new(), bufs: NameBufs::default() }
    }

    pub(crate) fn counts(&self) -> (u64, u64) {
        (self.listed, self.files)
    }

    fn name(&self, d: u32) -> &str {
        let x = &self.dirs[d as usize];
        &self.names[x.name_off as usize..x.name_off as usize + x.name_len as usize]
    }

    fn is(&self, d: u32, parent: u32, name: &str) -> bool {
        self.dirs[d as usize].parent == parent && eq_ci(self.name(d), name)
    }

    fn find_child(&self, parent: u32, name: &str) -> Option<u32> {
        if let Some(&d) = self.index.get(&child_key(parent, name))
            && self.is(d, parent, name)
        {
            return Some(d);
        }
        self.clashes.iter().copied().find(|&d| self.is(d, parent, name))
    }

    fn children(&self, d: u32) -> impl Iterator<Item = u32> + '_ {
        let mut c = self.dirs[d as usize].child;
        std::iter::from_fn(move || {
            (c != NONE).then(|| {
                let now = c;
                c = self.dirs[now as usize].next;
                now
            })
        })
    }

    /// A new, not yet listed subfolder. `None` for a name no file system
    /// would give.
    fn add_child(&mut self, parent: u32, name: &str) -> Option<u32> {
        if name.is_empty() || name.contains('\\') || name.len() > u16::MAX as usize || self.names.len() + name.len() > u32::MAX as usize {
            return None;
        }
        let d = Dir { parent, next: self.dirs[parent as usize].child, child: NONE, name_off: self.names.len() as u32, name_len: name.len() as u16, listed: false, nfiles: 0, pass: 0, listed_at: 0, files: None };
        self.names.push_str(name);
        let id = match self.free.pop() {
            Some(id) => {
                self.dirs[id as usize] = d;
                id
            }
            None => {
                if self.dirs.len() >= DEAD as usize {
                    return None;
                }
                self.dirs.push(d);
                (self.dirs.len() - 1) as u32
            }
        };
        self.dirs[parent as usize].child = id;
        match self.index.entry(child_key(parent, name)) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(id);
            }
            std::collections::hash_map::Entry::Occupied(_) => self.clashes.push(id),
        }
        Some(id)
    }

    /// Forget `top` (already unlinked from its parent) and all below it.
    fn drop_subtree(&mut self, top: u32, ch: &mut Change) {
        let mut stack = vec![top];
        while let Some(x) = stack.pop() {
            stack.extend(self.children(x));
            let key = child_key(self.dirs[x as usize].parent, self.name(x));
            if self.index.get(&key) == Some(&x) {
                self.index.remove(&key);
                if let Some(i) = self.clashes.iter().position(|&o| child_key(self.dirs[o as usize].parent, self.name(o)) == key) {
                    let o = self.clashes.swap_remove(i);
                    self.index.insert(key, o);
                }
            } else if let Some(i) = self.clashes.iter().position(|&o| o == x) {
                self.clashes.swap_remove(i);
            }
            let d = &mut self.dirs[x as usize];
            ch.dirs_removed += 1;
            ch.files_removed += d.nfiles;
            self.files -= d.nfiles as u64;
            self.listed -= d.listed as u64;
            self.names_dead += d.name_len as usize;
            *d = Dir { parent: DEAD, next: NONE, child: NONE, name_off: 0, name_len: 0, listed: false, nfiles: 0, pass: 0, listed_at: 0, files: None };
            self.free.push(x);
        }
    }

    /// Folder names of removed folders stay in `names` until they are half of it.
    fn tidy_names(&mut self) {
        if self.names_dead < 1 << 16 || self.names_dead * 2 < self.names.len() {
            return;
        }
        let mut fresh = String::with_capacity(self.names.len() - self.names_dead);
        for i in 0..self.dirs.len() {
            if self.dirs[i].parent == DEAD {
                continue;
            }
            let off = fresh.len() as u32;
            fresh.push_str(self.name(i as u32));
            self.dirs[i].name_off = off;
        }
        self.names = fresh;
        self.names_dead = 0;
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.dirs.shrink_to_fit();
        self.free.shrink_to_fit();
        self.names.shrink_to_fit();
        self.index.shrink_to_fit();
        self.scratch = Vec::new();
        self.bufs = NameBufs::default();
    }

    /// The folder at `rel` (relative to the root, `""` for the root).
    fn resolve(&self, rel: &str) -> Option<u32> {
        let mut d = 0u32;
        for comp in rel.split('\\').filter(|c| !c.is_empty()) {
            d = self.find_child(d, comp)?;
        }
        Some(d)
    }

    /// `path` relative to this root, when it is the root or inside it.
    fn rel<'p>(&self, path: &'p str) -> Option<&'p str> {
        rel_of(&self.root_lower, path)
    }

    fn chain(&self, d: u32, chain: &mut Vec<u32>) {
        chain.clear();
        let mut x = d;
        while x != 0 && x < DEAD {
            chain.push(x);
            x = self.dirs[x as usize].parent;
        }
    }

    fn path_of(&self, d: u32) -> String {
        let mut chain = Vec::new();
        self.chain(d, &mut chain);
        let mut out = self.root.clone();
        for &x in chain.iter().rev() {
            if !out.ends_with('\\') {
                out.push('\\');
            }
            out.push_str(self.name(x));
        }
        out
    }

    fn path_lower(&self, d: u32, out: &mut String, chain: &mut Vec<u32>) {
        self.chain(d, chain);
        out.clear();
        out.push_str(&self.root_lower);
        for &x in chain.iter().rev() {
            if !out.ends_with('\\') {
                out.push('\\');
            }
            push_lower(self.name(x), out);
        }
    }

    /// A file of folder `d` by name: the same spelling first, else the same
    /// name in another case.
    fn find_file(&self, d: u32, name: &str) -> Option<(String, u64, i64)> {
        let mut it = Entries::new(self.dirs[d as usize].files.as_deref()?);
        let mut other = None;
        while let Some((size, mtime)) = it.next() {
            if it.name == name {
                return Some((it.name, size, mtime));
            }
            if other.is_none() && eq_ci(&it.name, name) {
                other = Some((it.name.clone(), size, mtime));
            }
        }
        other
    }

    /// Replace the contents of the folder at `rel`. Sorts `files`. The flag
    /// says whether anything was altered: files, the folders known, or a
    /// folder listed for the first time (a later listing that finds the
    /// same is no change).
    pub(crate) fn apply(&mut self, rel: &str, pass: i64, listed_at: i64, subdirs: &[&str], files: &mut Vec<(&str, u64, i64)>) -> (Change, bool) {
        let mut ch = Change::default();
        let mut altered = false;
        let mut d = 0u32;
        for comp in rel.split('\\').filter(|c| !c.is_empty()) {
            d = match self.find_child(d, comp) {
                Some(c) => c,
                None => {
                    // The folder above was listed later and did not have it: this listing is of a folder since removed.
                    let up = &self.dirs[d as usize];
                    if up.listed && up.listed_at > listed_at {
                        return (ch, altered);
                    }
                    let Some(c) = self.add_child(d, comp) else { return (ch, altered) };
                    altered = true;
                    c
                }
            };
        }
        if self.dirs[d as usize].listed && listed_at < self.dirs[d as usize].listed_at {
            return (ch, altered);
        }

        // Subfolders: forget the ones no longer listed first, so the new ones take their places.
        let mut kept: HashSet<u32> = HashSet::with_capacity(subdirs.len());
        let mut fresh: Vec<&str> = Vec::new();
        for name in subdirs {
            match self.find_child(d, name) {
                Some(c) => {
                    kept.insert(c);
                }
                None => fresh.push(name),
            }
        }
        let (mut prev, mut c) = (NONE, self.dirs[d as usize].child);
        while c != NONE {
            let next = self.dirs[c as usize].next;
            if kept.contains(&c) {
                prev = c;
            } else {
                if prev == NONE {
                    self.dirs[d as usize].child = next;
                } else {
                    self.dirs[prev as usize].next = next;
                }
                self.drop_subtree(c, &mut ch);
            }
            c = next;
        }
        for name in fresh {
            // The same name twice in one listing is one folder.
            if self.find_child(d, name).is_none() && self.add_child(d, name).is_some() {
                altered = true;
            }
        }

        files.retain(|f| !f.0.is_empty());
        files.sort_unstable_by(|a, b| a.0.cmp(b.0));
        files.dedup_by(|a, b| a.0 == b.0);
        let (mut added, mut changed, mut removed) = (0u32, 0u32, 0u32);
        match self.dirs[d as usize].files.clone() {
            None => added = files.len() as u32,
            Some(blob) => {
                let mut it = Entries::new(&blob);
                let mut i = 0;
                while let Some((size, mtime)) = it.next() {
                    while i < files.len() && files[i].0 < it.name.as_str() {
                        added += 1;
                        i += 1;
                    }
                    if i < files.len() && files[i].0 == it.name {
                        changed += (files[i].1 != size || files[i].2 != mtime) as u32;
                        i += 1;
                    } else {
                        removed += 1;
                    }
                }
                added += (files.len() - i) as u32;
            }
        }
        if added + changed + removed > 0 {
            let blob = encode(files, &mut self.scratch, &mut self.bufs);
            let x = &mut self.dirs[d as usize];
            self.files = self.files + added as u64 - removed as u64;
            x.files = blob;
            x.nfiles = files.len() as u32;
        }
        let x = &mut self.dirs[d as usize];
        let first = !x.listed;
        self.listed += first as u64;
        x.listed = true;
        x.pass = pass;
        x.listed_at = listed_at;
        ch.files_added += added;
        ch.files_changed += changed;
        ch.files_removed += removed;
        self.tidy_names();
        (ch, altered || first || !ch.is_empty())
    }

    /// Which folders a scope lets a search look in.
    fn in_scope(&self, roots: &[String]) -> InScope {
        if roots.is_empty() {
            return InScope::All;
        }
        let mut marks: Vec<bool> = Vec::new();
        for r in roots {
            if inside(&self.root_lower, r) {
                return InScope::All;
            }
            let Some(top) = self.rel(r).and_then(|rest| self.resolve(rest)) else { continue };
            marks.resize(self.dirs.len(), false);
            let mut stack = vec![top];
            while let Some(x) = stack.pop() {
                marks[x as usize] = true;
                stack.extend(self.children(x));
            }
        }
        InScope::Some(marks)
    }

    /// `top` and everything below it, parents before children.
    pub(crate) fn snapshot(&self, top: u32) -> Snap {
        let mut snap = Snap { top: self.path_of(top), names: String::new(), dirs: Vec::new() };
        let mut stack = vec![(top, 0u32)];
        while let Some((x, depth)) = stack.pop() {
            let d = &self.dirs[x as usize];
            let off = snap.names.len();
            if depth > 0 {
                snap.names.push_str(self.name(x));
            }
            snap.dirs.push(SnapDir { depth, name: (off, snap.names.len()), listed: d.listed, pass: d.pass, listed_at: d.listed_at, nfiles: d.nfiles, files: d.files.clone() });
            stack.extend(self.children(x).map(|c| (c, depth + 1)));
        }
        snap
    }

    fn same_stem(&self, d: u32, stem: &str, but: Option<&str>, out: &mut Vec<FileInfo>) {
        let Some(blob) = self.dirs[d as usize].files.as_deref() else { return };
        let mut it = Entries::new(blob);
        let mut dir = None;
        while let Some((size, mtime)) = it.next() {
            if but.is_some_and(|b| eq_ci(b, &it.name)) || stem_key(&it.name) != stem {
                continue;
            }
            let dir = dir.get_or_insert_with(|| self.path_of(d));
            out.push(FileInfo { path: join(dir, &it.name), name: it.name.clone(), size, mtime });
        }
    }
}

enum InScope {
    All,
    Some(Vec<bool>),
}

impl InScope {
    fn has(&self, d: usize) -> bool {
        match self {
            InScope::All => true,
            InScope::Some(m) => m.get(d).copied().unwrap_or(false),
        }
    }
}

/// A folder and all below it as they were at one moment, parents before
/// children, so that long work (writing a catalog file, a caller's own
/// loop) holds no lock.
pub(crate) struct Snap {
    top: String,
    names: String,
    pub(crate) dirs: Vec<SnapDir>,
}

pub(crate) struct SnapDir {
    /// 0 for the top folder.
    pub(crate) depth: u32,
    name: (usize, usize),
    pub(crate) listed: bool,
    pub(crate) pass: i64,
    pub(crate) listed_at: i64,
    pub(crate) nfiles: u32,
    pub(crate) files: Option<Arc<[u8]>>,
}

impl Snap {
    pub(crate) fn name(&self, i: usize) -> &str {
        let (a, b) = self.dirs[i].name;
        &self.names[a..b]
    }

    /// The subfolders of folder `i`.
    pub(crate) fn children(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        let depth = self.dirs[i].depth;
        (i + 1..self.dirs.len()).take_while(move |&j| self.dirs[j].depth > depth).filter(move |&j| self.dirs[j].depth == depth + 1)
    }

    /// Every folder with its full path, in order; `f` returns false to stop.
    pub(crate) fn walk(&self, f: &mut dyn FnMut(usize, &str) -> bool) -> bool {
        let mut path = self.top.clone();
        let mut ends = vec![path.len()];
        for (i, d) in self.dirs.iter().enumerate() {
            let depth = d.depth as usize;
            if depth > 0 {
                path.truncate(ends[depth - 1]);
                if !path.ends_with('\\') {
                    path.push('\\');
                }
                path.push_str(self.name(i));
                ends.truncate(depth);
                ends.push(path.len());
            }
            if !f(i, &path) {
                return false;
            }
        }
        true
    }

    fn each_file(&self, f: &mut dyn FnMut(&str, &str, u64, i64) -> bool) -> bool {
        self.walk(&mut |i, dir| {
            let Some(blob) = self.dirs[i].files.as_deref() else { return true };
            let mut it = Entries::new(blob);
            while let Some((size, mtime)) = it.next() {
                if !f(dir, &it.name, size, mtime) {
                    return false;
                }
            }
            true
        })
    }
}

/// Buffers for the forms of a name.
#[derive(Default)]
struct NameBufs {
    lower: String,
    sep: Vec<u8>,
}

/// A file name (or a folder path) in the forms a search compares, each
/// worked out only when asked for, and once. Most names are dismissed on
/// one form alone.
struct Forms<'a> {
    text: &'a str,
    /// `text` is lower case already (a folder path).
    is_lower: bool,
    /// Nearly every name is: compared as stored, without a lower-case copy.
    ascii: bool,
    bufs: &'a mut NameBufs,
    has_lower: bool,
    has_sep: bool,
}

impl<'a> Forms<'a> {
    fn of(text: &'a str, is_lower: bool, bufs: &'a mut NameBufs) -> Forms<'a> {
        Forms { text, is_lower, ascii: text.is_ascii(), bufs, has_lower: false, has_sep: false }
    }

    fn lower(&mut self) -> &str {
        if self.is_lower {
            return self.text;
        }
        if !self.has_lower {
            lower_into(self.text, &mut self.bufs.lower);
            self.has_lower = true;
        }
        &self.bufs.lower
    }

    /// Letters and digits only, lower case (as `analyzer::sep_form`).
    fn sep(&mut self) -> &[u8] {
        if !self.has_sep {
            if self.ascii {
                let sep = &mut self.bufs.sep;
                sep.clear();
                sep.resize(self.text.len(), 0);
                let mut n = 0;
                for b in self.text.bytes() {
                    sep[n] = b.to_ascii_lowercase();
                    n += b.is_ascii_alphanumeric() as usize;
                }
                sep.truncate(n);
            } else if self.is_lower {
                sep_into(self.text, &mut self.bufs.sep);
            } else {
                self.lower();
                let NameBufs { lower, sep } = &mut *self.bufs;
                sep_into(lower, sep);
            }
            self.has_sep = true;
        }
        &self.bufs.sep
    }

    /// Whether `raw` (lower case) is in the name as typed.
    fn holds(&mut self, raw: &str) -> bool {
        if self.ascii && !self.is_lower { has_ci(self.text.as_bytes(), raw.as_bytes()) } else { has(self.lower().as_bytes(), raw.as_bytes()) }
    }

    /// Whether the name is `raw`, with or without its extension.
    fn is_whole(&mut self, raw: &str) -> bool {
        if self.ascii && raw.is_ascii() {
            let (n, len) = (self.text, raw.len());
            return n.len() >= len && n[..len].eq_ignore_ascii_case(raw) && (n.len() == len || n.rfind('.') == Some(len));
        }
        let lower = self.lower();
        lower == raw || lower.rsplit_once('.').is_some_and(|(stem, _)| stem == raw)
    }
}

/// Whether the name ends with `ext` (lower case, with its dot).
fn has_ext(name: &mut Forms, ext: &str) -> bool {
    if name.ascii {
        let (n, e) = (name.text.as_bytes(), ext.as_bytes());
        n.len() >= e.len() && n[n.len() - e.len()..].eq_ignore_ascii_case(e)
    } else {
        name.lower().ends_with(ext)
    }
}

/// What one name scan looks for.
struct Want<'t> {
    term: &'t Term,
    pattern: Option<String>,
    /// Matched only as typed: punctuation, a letter or digit or two with
    /// punctuation, or a single letter or digit (which dropping separators
    /// cannot bring together with anything).
    literal_only: bool,
    /// Typed as letters and digits only.
    plain: bool,
    /// Pairs every matching name holds (see [`Grams`]).
    grams: Vec<u32>,
}

impl<'t> Want<'t> {
    fn new(term: &'t Term) -> Want<'t> {
        let mut grams = Vec::new();
        if term.has_wildcards() {
            // Each stretch between wildcards is in the name as typed.
            term.raw.split(['*', '?']).for_each(|run| push_grams(sep_form(run).as_bytes(), &mut grams));
        } else {
            push_grams(term.sep.as_bytes(), &mut grams);
        }
        Want { term, pattern: term.has_wildcards().then(|| format!("*{}*", term.raw)), literal_only: term.is_literal_only() || term.is_punctuation() || (term.sep == term.raw && term.sep.chars().count() == 1), plain: term.sep == term.raw, grams }
    }

    /// 0 the whole name, 1 the text as typed, 2 with other separators or a
    /// pattern; `None` when `explain_name` could not match this name.
    fn group(&self, name: &mut Forms) -> Option<u8> {
        if let Some(p) = &self.pattern {
            let lower = name.lower();
            let hit = if p.is_ascii() && lower.is_ascii() { glob_ascii(p.as_bytes(), lower.as_bytes()) } else { glob_match(p, lower) };
            return hit.then_some(2);
        }
        let raw = self.term.raw.as_str();
        let as_typed = |name: &mut Forms| name.holds(raw).then(|| if name.is_whole(raw) { 0 } else { 1 });
        if self.literal_only || self.term.sep.is_empty() {
            return as_typed(name);
        }
        // Typed without separators (`pdf`, `psv`), a term is most often in
        // the name just so: that needs no second form of the name.
        if self.plain
            && let Some(g) = as_typed(name)
        {
            return Some(g);
        }
        // The text as typed has the term's letters and digits too, so a
        // name without them is no match of either kind: most stop here.
        if !has(name.sep(), self.term.sep.as_bytes()) {
            return None;
        }
        if self.plain { Some(2) } else { Some(as_typed(name).unwrap_or(2)) }
    }
}

/// The lower-case path of the folder being scanned, worked out only when a
/// matcher asks for it (a plain name search never does).
struct LazyDir<'a> {
    tree: &'a Tree,
    dir: u32,
    buf: &'a mut String,
    chain: &'a mut Vec<u32>,
    filled: bool,
    /// A backslash was appended for [`LazyDir::with_slash`].
    slashed: bool,
}

impl LazyDir<'_> {
    fn fill(&mut self) {
        if !self.filled {
            self.tree.path_lower(self.dir, self.buf, self.chain);
            self.filled = true;
        }
    }

    /// As a folder path is written: `\\srv\p\rev-1`, `c:\`.
    fn plain(&mut self) -> &str {
        self.fill();
        &self.buf[..self.buf.len() - self.slashed as usize]
    }

    /// Always ending in a backslash, so a fragment like `rev-1\` can match.
    fn with_slash(&mut self) -> &str {
        self.fill();
        if !self.buf.ends_with('\\') {
            self.buf.push('\\');
            self.slashed = true;
        }
        self.buf
    }
}

/// How a scan decides: once per folder, then per file name. `bufs` is
/// for the forms of the folder path.
trait Matcher: Sync {
    type InDir;
    /// `None` skips the folder.
    fn dir(&self, dir: &mut LazyDir, grams: &Grams, bufs: &mut NameBufs) -> Option<Self::InDir>;
    /// The group the file sorts in (lower first), `None` when it does not match.
    fn file(&self, st: &Self::InDir, name: &mut Forms, dir: &mut LazyDir, bufs: &mut NameBufs) -> Option<u8>;
}

struct OneTerm<'t>(Want<'t>);

impl Matcher for OneTerm<'_> {
    type InDir = ();
    fn dir(&self, _: &mut LazyDir, grams: &Grams, _: &mut NameBufs) -> Option<()> {
        grams.has_all(&self.0.grams).then_some(())
    }
    fn file(&self, _: &(), name: &mut Forms, _: &mut LazyDir, _: &mut NameBufs) -> Option<u8> {
        self.0.group(name)
    }
}

/// Every term in the name or the folder path.
struct AllTerms<'t>(Vec<Want<'t>>);

/// Which terms the folder path holds, each looked for once per folder.
#[derive(Default)]
struct InPath {
    known: Cell<u64>,
    yes: Cell<u64>,
}

impl AllTerms<'_> {
    fn in_path(&self, i: usize, st: &InPath, dir: &mut LazyDir, bufs: &mut NameBufs) -> bool {
        let bit = 1u64 << (i % 64);
        if i < 64 && st.known.get() & bit != 0 {
            return st.yes.get() & bit != 0;
        }
        let hit = self.0[i].group(&mut Forms::of(dir.plain(), true, bufs)).is_some();
        if i < 64 {
            st.known.set(st.known.get() | bit);
            st.yes.set(st.yes.get() | if hit { bit } else { 0 });
        }
        hit
    }
}

impl Matcher for AllTerms<'_> {
    type InDir = InPath;
    fn dir(&self, dir: &mut LazyDir, grams: &Grams, bufs: &mut NameBufs) -> Option<InPath> {
        let st = InPath::default();
        // A term no name here can hold must be in the folder path.
        for (i, w) in self.0.iter().enumerate() {
            if !grams.has_all(&w.grams) && !self.in_path(i, &st, dir, bufs) {
                return None;
            }
        }
        Some(st)
    }
    fn file(&self, st: &InPath, name: &mut Forms, dir: &mut LazyDir, bufs: &mut NameBufs) -> Option<u8> {
        let mut best = 2u8;
        for (i, w) in self.0.iter().enumerate() {
            match w.group(name) {
                Some(g) => best = best.min(g.max(1)),
                None if self.in_path(i, st, dir, bufs) => {}
                None => return None,
            }
        }
        Some(best)
    }
}

/// A pasted path or a piece of one: files whose full path contains it.
struct PathText {
    /// Lower case.
    text: String,
    /// `text` without trailing backslashes: what a folder it names ends with.
    folder: String,
    /// `text` cut after its last backslash when something follows: the
    /// folder part (with that backslash) and the start of a file name.
    split: Option<(String, String)>,
}

struct PathIn {
    /// The text is in the folder's own path, so every file below matches:
    /// group 2 directly in a folder the text names, else 3.
    all: Option<u8>,
    /// The folder ends where the text's folder part ends, so a file name
    /// may complete the text. `true` when that part is the whole folder path.
    names: Option<bool>,
}

impl Matcher for PathText {
    type InDir = PathIn;

    fn dir(&self, dir: &mut LazyDir, _: &Grams, _: &mut NameBufs) -> Option<PathIn> {
        let ds = dir.with_slash();
        let all = ds.contains(self.text.as_str()).then(|| if ds[..ds.len() - 1].ends_with(self.folder.as_str()) { 2 } else { 3 });
        let names = match &self.split {
            Some((folder, _)) => ds.ends_with(folder.as_str()).then_some(ds.len() == folder.len()),
            // No backslash in the text: it can only be inside a name.
            None => (!self.text.contains('\\')).then_some(false),
        };
        (all.is_some() || names.is_some()).then_some(PathIn { all, names })
    }

    fn file(&self, st: &PathIn, name: &mut Forms, _: &mut LazyDir, _: &mut NameBufs) -> Option<u8> {
        if let Some(whole) = st.names {
            let name_lower = name.lower();
            match &self.split {
                Some((_, rest)) if name_lower == rest => return Some(if whole { 0 } else { 1 }),
                Some((_, rest)) if st.all.is_none() => return name_lower.starts_with(rest.as_str()).then_some(3),
                None if name_lower.ends_with(self.text.as_str()) => return Some(1),
                None if st.all.is_none() => return name_lower.contains(self.text.as_str()).then_some(3),
                _ => {}
            }
        }
        st.all
    }
}

/// One match of a scan: enough to sort it and to read the file back.
struct Hit {
    grp: u8,
    /// Entries after `at`.
    k: u8,
    tree: u16,
    dir: u32,
    /// A restart point in the folder's block.
    at: u32,
    mtime: i64,
}

fn hit_key(h: &Hit) -> (u8, Reverse<i64>) {
    (h.grp, Reverse(h.mtime))
}

/// Keep the best `cap` (unordered); `cap` 0 keeps all.
fn keep_best(found: &mut Vec<Hit>, cap: usize) {
    if cap > 0 && found.len() > cap {
        found.select_nth_unstable_by_key(cap - 1, hit_key);
        found.truncate(cap);
    }
}

/// Folders handed to a scanning thread at a time.
const BLOCK: usize = 128;

/// Every root this PC can see, shared as `Arc<Catalog>`.
#[derive(Default)]
pub struct Catalog {
    trees: RwLock<Vec<Tree>>,
    generation: AtomicU64,
}

impl Catalog {
    pub fn new() -> Catalog {
        Catalog::default()
    }

    // A panic while the lock was held leaves the catalog as that listing
    // left it: stale at worst, so keep serving it.
    fn r(&self) -> RwLockReadGuard<'_, Vec<Tree>> {
        self.trees.read().unwrap_or_else(|e| e.into_inner())
    }

    fn w(&self) -> RwLockWriteGuard<'_, Vec<Tree>> {
        self.trees.write().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Register a root (`\\10.0.0.5\project` or `C:\`); listings and lookups
    /// outside every root are ignored.
    pub fn add_root(&self, root: &str) {
        let root = norm_dir(root);
        if root.is_empty() {
            return;
        }
        let mut g = self.w();
        if !g.iter().any(|t| eq_ci(&t.root, &root)) {
            g.push(Tree::new(&root));
            self.bump();
        }
    }

    pub fn remove_root(&self, root: &str) {
        let root = norm_dir(root);
        let gone: Vec<Tree> = {
            let mut g = self.w();
            let (gone, kept) = std::mem::take(&mut *g).into_iter().partition(|t| eq_ci(&t.root, &root));
            *g = kept;
            gone
        };
        // Freed after the lock is released: a large root takes a moment.
        if !gone.is_empty() {
            self.bump();
        }
    }

    pub fn roots(&self) -> Vec<String> {
        self.r().iter().map(|t| t.root.clone()).collect()
    }

    /// The root holding `path` (the longest, should roots nest) and the
    /// path relative to it.
    fn locate<'p>(trees: &[Tree], path: &'p str) -> Option<(usize, &'p str)> {
        trees.iter().enumerate().filter_map(|(i, t)| t.rel(path).map(|rel| (i, rel))).max_by_key(|&(i, _)| trees[i].root.len())
    }

    /// A file path as its folder and name inside a root.
    fn locate_file<'p>(trees: &[Tree], path: &'p str) -> Option<(usize, u32, &'p str)> {
        let (i, rel) = Self::locate(trees, path)?;
        let (dir, name) = rel.rsplit_once('\\').unwrap_or(("", rel));
        if name.is_empty() {
            return None;
        }
        Some((i, trees[i].resolve(dir)?, name))
    }

    /// Replace one folder's contents. Files and subfolders no longer listed
    /// go (subfolders with everything under them). A listing older than what
    /// the folder already holds (`listed_at`) is ignored, as is one of a
    /// folder that a later listing of the folder above no longer has.
    pub fn apply(&self, l: &Listing) -> Change {
        let dir = norm_dir(&l.dir);
        let subdirs: Vec<&str> = l.subdirs.iter().map(String::as_str).collect();
        let mut files: Vec<(&str, u64, i64)> = l.files.iter().map(|(n, s, m)| (n.as_str(), *s, *m)).collect();
        let mut g = self.w();
        let Some((i, rel)) = Self::locate(&g, &dir) else { return Change::default() };
        let (ch, altered) = g[i].apply(rel, l.pass, l.listed_at, &subdirs, &mut files);
        drop(g);
        if altered {
            self.bump();
        }
        ch
    }

    /// Run `f` on one root's tree under the write lock; it says whether it
    /// altered anything. `None` when the root is not registered.
    pub(crate) fn with_tree<R>(&self, root: &str, f: impl FnOnce(&mut Tree) -> (R, bool)) -> Option<R> {
        let mut g = self.w();
        let t = g.iter_mut().find(|t| eq_ci(&t.root, root))?;
        let (r, altered) = f(t);
        drop(g);
        if altered {
            self.bump();
        }
        Some(r)
    }

    /// Put a tree built aside in place of its root (or add it), in one step,
    /// so no search ever sees a half-loaded root.
    pub(crate) fn swap_root(&self, tree: Tree) {
        let old = {
            let mut g = self.w();
            match g.iter().position(|t| eq_ci(&t.root, &tree.root)) {
                Some(i) => Some(std::mem::replace(&mut g[i], tree)),
                None => {
                    g.push(tree);
                    None
                }
            }
        };
        self.bump();
        drop(old);
    }

    pub(crate) fn snapshot_root(&self, root: &str) -> Option<Snap> {
        self.r().iter().find(|t| eq_ci(&t.root, root)).map(|t| t.snapshot(0))
    }

    /// Give back memory kept for growth (after a bulk load).
    pub fn shrink_to_fit(&self) {
        self.w().iter_mut().for_each(Tree::shrink_to_fit);
    }

    pub fn file_count(&self) -> u64 {
        self.r().iter().map(|t| t.files).sum()
    }

    /// Folders whose contents are held. A subfolder a listing only named is
    /// known ([`Catalog::contains_dir`]) but not counted until it is listed.
    pub fn dir_count(&self) -> u64 {
        self.r().iter().map(|t| t.listed).sum()
    }

    /// Listed folders and files of one root.
    pub fn root_counts(&self, root: &str) -> (u64, u64) {
        let root = norm_dir(root);
        self.r().iter().find(|t| eq_ci(&t.root, &root)).map_or((0, 0), Tree::counts)
    }

    /// Bumped on every change that altered anything.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn get(&self, path: &str) -> Option<FileInfo> {
        let g = self.r();
        let (i, d, name) = Self::locate_file(&g, path)?;
        let (name, size, mtime) = g[i].find_file(d, name)?;
        Some(FileInfo { path: join(&g[i].path_of(d), &name), name, size, mtime })
    }

    /// Whether the folder is known: listed, or named by a listing of the
    /// folder above it.
    pub fn contains_dir(&self, dir: &str) -> bool {
        let dir = norm_dir(dir);
        let g = self.r();
        Self::locate(&g, &dir).is_some_and(|(i, rel)| g[i].resolve(rel).is_some())
    }

    /// Whether the folder itself has been listed at least once (not merely
    /// named by its parent's listing): a file missing from a listed folder
    /// is gone, one missing from an unlisted folder is not known yet.
    pub fn dir_listed(&self, dir: &str) -> bool {
        let dir = norm_dir(dir);
        let g = self.r();
        Self::locate(&g, &dir).and_then(|(i, rel)| Some(g[i].dirs[g[i].resolve(rel)? as usize].listed)).unwrap_or(false)
    }

    /// Subfolder paths when `dir` was listed in `pass` (walk resumption).
    pub fn listed_in_pass(&self, dir: &str, pass: i64) -> Option<Vec<String>> {
        let dir = norm_dir(dir);
        let g = self.r();
        let (i, rel) = Self::locate(&g, &dir)?;
        let t = &g[i];
        let d = t.resolve(rel)?;
        let x = &t.dirs[d as usize];
        if !x.listed || x.pass != pass {
            return None;
        }
        let here = t.path_of(d);
        Some(t.children(d).map(|c| join(&here, t.name(c))).collect())
    }

    /// Name search, as `namemem` did: whole names first, then the text as
    /// typed, then other separators / patterns; newer first within each.
    /// At most `cap` (0: no limit).
    pub fn candidates(&self, term: &Term, scope: &NameScope, cap: usize) -> Vec<FileInfo> {
        self.scan(scope, cap, &OneTerm(Want::new(term)))
    }

    /// Files where every term is in the name or the folder path.
    pub fn candidates_all(&self, terms: &[Term], scope: &NameScope, cap: usize) -> Vec<FileInfo> {
        self.scan(scope, cap, &AllTerms(terms.iter().map(Want::new).collect()))
    }

    /// Files whose full path (folder, `\`, name) contains `text`, without
    /// case: a whole file path, a folder path with or without its trailing
    /// backslash (every file below it), or a fragment, which may run from a
    /// folder into a file name (`2632\14-01-15-2632_PSV`). The file whose
    /// path is the text first, then files whose path ends with it, then
    /// files directly in a folder the text names, then the rest; newer
    /// first within each; at most `cap`.
    pub fn candidates_path(&self, text: &str, scope: &NameScope, cap: usize) -> Vec<FileInfo> {
        let text = lower(text.trim().trim_matches('"'));
        if text.is_empty() {
            return Vec::new();
        }
        let split = text.rsplit_once('\\').filter(|(_, name)| !name.is_empty()).map(|(folder, name)| (format!("{folder}\\"), name.to_string()));
        let folder = text.trim_end_matches('\\').to_string();
        self.scan(scope, cap, &PathText { text, folder, split })
    }

    fn scan<M: Matcher>(&self, scope: &NameScope, cap: usize, m: &M) -> Vec<FileInfo> {
        let g = self.r();
        let roots: Vec<String> = scope.roots.iter().map(|r| lower(r.trim_end_matches('\\'))).collect();
        let scopes: Vec<InScope> = g.iter().map(|t| t.in_scope(&roots)).collect();
        let ext = scope.ext.as_ref().map(|e| format!(".{}", lower(e.trim_start_matches('.'))));
        let mut jobs: Vec<(usize, usize, usize)> = Vec::new();
        for (i, t) in g.iter().enumerate() {
            if matches!(&scopes[i], InScope::Some(m) if m.is_empty()) {
                continue;
            }
            jobs.extend((0..t.dirs.len()).step_by(BLOCK).map(|a| (i, a, (a + BLOCK).min(t.dirs.len()))));
        }
        let next = AtomicUsize::new(0);
        let work = || {
            let (mut name_bufs, mut dir_bufs, mut dir_lower, mut chain) = (NameBufs::default(), NameBufs::default(), String::new(), Vec::new());
            let mut out: Vec<Hit> = Vec::new();
            while let Some(&(ti, a, b)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                let tree = &g[ti];
                for d in a..b {
                    let x = &tree.dirs[d];
                    let Some(blob) = x.files.as_deref() else { continue };
                    if x.parent == DEAD || !scopes[ti].has(d) {
                        continue;
                    }
                    let mut lazy = LazyDir { tree, dir: d as u32, buf: &mut dir_lower, chain: &mut chain, filled: false, slashed: false };
                    let Some(st) = m.dir(&mut lazy, &Grams::of(blob).0, &mut dir_bufs) else { continue };
                    let mut it = Entries::new(blob);
                    while let Some((_, mtime)) = it.next() {
                        let mut name = Forms::of(&it.name, false, &mut name_bufs);
                        if ext.as_ref().is_some_and(|e| !has_ext(&mut name, e)) {
                            continue;
                        }
                        if let Some(grp) = m.file(&st, &mut name, &mut lazy, &mut dir_bufs) {
                            out.push(Hit { grp, k: it.k, tree: ti as u16, dir: d as u32, at: it.restart, mtime });
                        }
                    }
                    // A common word matches millions: never hold more than a few times what is wanted.
                    if cap > 0 && out.len() > cap.max(1024) * 4 {
                        keep_best(&mut out, cap);
                    }
                }
            }
            out
        };
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16).min(jobs.len());
        let mut found: Vec<Hit> = if threads <= 1 {
            work()
        } else {
            std::thread::scope(|s| {
                let parts: Vec<_> = (0..threads).map(|_| s.spawn(work)).collect();
                parts.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
            })
        };
        keep_best(&mut found, cap);
        found.sort_by_key(hit_key);
        let mut dir_of: (usize, u32, String) = (usize::MAX, 0, String::new());
        found
            .into_iter()
            .filter_map(|h| {
                let t = &g[h.tree as usize];
                let mut it = Entries::at(t.dirs[h.dir as usize].files.as_deref()?, h.at);
                let mut last = it.next()?;
                for _ in 0..h.k {
                    last = it.next()?;
                }
                if (dir_of.0, dir_of.1) != (h.tree as usize, h.dir) {
                    dir_of = (h.tree as usize, h.dir, t.path_of(h.dir));
                }
                Some(FileInfo { path: join(&dir_of.2, &it.name), name: it.name, size: last.0, mtime: last.1 })
            })
            .collect()
    }

    /// Every file under `folder` (a root or any folder below one), in no
    /// promised order. `f` returns false to stop. The files are those held
    /// at the moment of the call; no lock is held while `f` runs, so it may
    /// use the catalog itself.
    pub fn for_each_under(&self, folder: &str, f: &mut dyn FnMut(&str /*dir*/, &str /*name*/, u64, i64) -> bool) {
        let folder = norm_dir(folder);
        let snaps: Vec<Snap> = {
            let g = self.r();
            match Self::locate(&g, &folder) {
                Some((i, rel)) => g[i].resolve(rel).map(|d| g[i].snapshot(d)).into_iter().collect(),
                None => {
                    // Above the roots (`\\10.0.0.5`): every root below it.
                    let above = lower(folder.trim_end_matches('\\'));
                    g.iter().filter(|t| !above.is_empty() && inside(&t.root_lower, &above)).map(|t| t.snapshot(0)).collect()
                }
            }
        };
        for s in &snaps {
            if !s.each_file(f) {
                return;
            }
        }
    }

    /// Files with the same name stem in the same folder and in sibling
    /// folders (children of the same parent), the file itself excluded.
    pub fn twins(&self, path: &str) -> Vec<FileInfo> {
        let g = self.r();
        let Some((i, d, name)) = Self::locate_file(&g, path) else { return Vec::new() };
        let t = &g[i];
        let stem = stem_key(name);
        let mut out = Vec::new();
        t.same_stem(d, &stem, Some(name), &mut out);
        let up = t.dirs[d as usize].parent;
        if up < DEAD {
            for sib in t.children(up).filter(|&s| s != d) {
                t.same_stem(sib, &stem, None, &mut out);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
