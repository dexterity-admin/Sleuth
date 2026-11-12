//! The SQLite schema of what was read, used three ways (see
//! `docs/design/office-store.md`):
//!
//! - a **part**: a write-once file in the office store's `text` folder,
//!   opened read-only and immutable by every PC ([`Store::open_part`]);
//! - `work.db`: this PC's readings not yet published as a part;
//! - `local.db`: this PC's small bookkeeping (history, events, retries).
//!
//! File names are not kept here any more: they live in the in-memory
//! catalog (`sleuth-office`). `files` holds only documents that were read
//! (or could not be).
//!
//! - `files`: one row per read document, keyed by canonical path.
//! - `names`: trigram full-text index over the folded names of those.
//! - `lines` + `lines_fts`: every line read from a PDF (text layer, OCR or a
//!   person's correction) with its page and box, and a trigram index over the
//!   folded text.
//! - `pages`: how each page was classified and read.
//! - `engineering_tags` + `tag_occurrences`: tags found by the tag parser,
//!   with the raw reading, confidence and position of every occurrence.
//! - `drawing_metadata`: title-block details with the evidence for each.
//! - `corrections`: readings checked by a person; the original stays.
//!
//! Substring search on the folded form finds every candidate, because the
//! exact and separator-free forms are both contained in it; the precise
//! [`MatchClass`] is then decided in Rust by [`Term::explain`]. Folder,
//! project and metadata limits are applied in SQL, before any row limit.
//!
//! Schema changes are additive and `user_version` stays 1, so Sleuth 0.1
//! (after a rollback) can still open the file; the real schema level is in
//! `meta.schema`.

use crate::derive::Deriver;
use crate::text::{FileText, Line};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use sleuth_core::analyzer::{MatchClass, MatchDetail, Term, fold_form, glob_match, looks_like_path, norm_form, sep_form};
use sleuth_core::protocol::{DrawingMeta, NormBox, PageInfo, Provenance, TextSource};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// `PRAGMA user_version`: what Sleuth 0.1 checks. Never raised again.
pub const SCHEMA_VERSION: i64 = 1;
/// Additive schema level in `meta.schema`.
pub const SCHEMA_LEVEL: i64 = 5;
/// Removed files keep their readings this long, so a moved or renamed
/// drawing is not read again.
pub const GONE_KEEP_SECS: i64 = 7 * 24 * 3600;
/// A reading job left this long by a stopped worker is free again.
pub const LEASE_SECS: i64 = 30 * 60;

/// `files.read_state`
pub mod state {
    pub const NOT_PDF: i64 = 0;
    pub const WAITING: i64 = 1;
    pub const READ: i64 = 2;
    pub const FAILED: i64 = 3;
    /// A PDF outside the folders chosen for reading.
    pub const UNREAD: i64 = 4;
    /// No longer on disk; readings kept for a while for moved copies.
    pub const GONE: i64 = 5;
    /// Read by the office server instead of this PC.
    pub const OFFICE: i64 = 6;
    /// Opened and found to need OCR (scanned pages): any PC's OCR lane may
    /// take it without opening it first to find out.
    pub const NEEDS_OCR: i64 = 7;
}

pub struct Store {
    pub conn: Connection,
}

/// One reading as the pool needs it to know what a part covers.
#[derive(Debug, Clone)]
pub struct ReadingRow {
    pub id: i64,
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    /// [`state::READ`] or [`state::FAILED`].
    pub state: i64,
    pub read_at: i64,
    pub error: String,
}

/// `file:` URI of a path for SQLite, so `immutable=1` can be asked for.
/// A network path keeps its two leading slashes (`file:////host/share/x.db`).
fn sqlite_uri(path: &Path, params: &str) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:");
    if p.starts_with("//") {
        out.push_str("//");
    } else if !p.starts_with('/') {
        out.push('/');
    }
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'.' | b'-' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    format!("{out}?{params}")
}

#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub read_state: i64,
}

/// One matching line or file name, before grouping per file.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub file_id: i64,
    pub page: Option<u32>,
    pub text: String,
    pub conf: f32,
    pub source: TextSource,
    pub bbox: Option<NormBox>,
    pub class: MatchClass,
    pub why: String,
}

/// Where a search may look, as SQL on `files f` (and `drawing_metadata m`).
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Folder or file paths (UNC for network locations); empty = everywhere.
    pub roots: Vec<String>,
    pub ext: Option<String>,
    /// Revision, separators ignored, upper case.
    pub rev: Option<String>,
    /// Drawing number contains (normalised).
    pub dwg: Option<String>,
    pub disc: Option<String>,
}

impl Scope {
    pub fn roots(roots: &[String]) -> Scope {
        Scope { roots: roots.to_vec(), ..Default::default() }
    }

    /// ` AND (...)` for `files f`, with its parameters.
    pub fn sql(&self) -> (String, Vec<String>) {
        let mut sql = String::new();
        let mut p: Vec<String> = Vec::new();
        if !self.roots.is_empty() {
            let mut parts = Vec::new();
            for r in &self.roots {
                let r = r.trim_end_matches('\\');
                p.push(r.to_string());
                p.push(format!("{}\\\\%", like_escape(r)));
                parts.push(format!("(f.path=?{} OR f.path LIKE ?{} ESCAPE '\\')", p.len() - 1, p.len()));
            }
            sql.push_str(&format!(" AND ({})", parts.join(" OR ")));
        }
        if let Some(e) = &self.ext {
            p.push(format!("%.{}", like_escape(e)));
            sql.push_str(&format!(" AND f.name LIKE ?{} ESCAPE '\\'", p.len()));
        }
        let mut meta = Vec::new();
        if let Some(r) = &self.rev {
            p.push(norm_form(r));
            meta.push(format!("m.revision_norm=?{}", p.len()));
        }
        if let Some(d) = &self.dwg {
            p.push(format!("%{}%", like_escape(&norm_form(d))));
            meta.push(format!("m.drawing_norm LIKE ?{} ESCAPE '\\'", p.len()));
        }
        if let Some(d) = &self.disc {
            p.push(d.clone());
            meta.push(format!("lower(m.discipline)=?{}", p.len()));
        }
        if !meta.is_empty() {
            sql.push_str(&format!(" AND f.id IN (SELECT m.file_id FROM drawing_metadata m WHERE {})", meta.join(" AND ")));
        }
        (sql, p)
    }

    /// Shift the `?N` placeholders of [`Scope::sql`] by `by`.
    pub(crate) fn sql_from(&self, by: usize) -> (String, Vec<String>) {
        let (mut sql, p) = self.sql();
        for i in (1..=p.len()).rev() {
            sql = sql.replace(&format!("?{i}"), &format!("?#{}", i + by));
        }
        (sql.replace("?#", "?"), p)
    }
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// A `*`/`?` pattern as the body of a LIKE pattern (escape `\`).
fn like_glob(s: &str) -> String {
    like_escape(s).replace('*', "%").replace('?', "_")
}

/// One character of a case-insensitive GLOB pattern: `[еЕ]` for a letter
/// with two cases, `[*]` for GLOB's own special characters, and `*`/`?`
/// passed through when `wild` (the term's own wildcards).
fn glob_char(c: char, wild: bool) -> String {
    if wild && (c == '*' || c == '?') {
        return c.to_string();
    }
    let up: String = c.to_uppercase().collect();
    let low: String = c.to_lowercase().collect();
    match c {
        '*' | '?' | '[' => format!("[{c}]"),
        _ if up.chars().count() == 1 && low.chars().count() == 1 && up != low => format!("[{low}{up}]"),
        _ => c.to_string(),
    }
}

/// Every spelling of `s` with each letter in either case (`об` -> `об`,
/// `Об`, `оБ`, `ОБ`); `None` when there would be more than 16.
fn case_variants(s: &str) -> Option<Vec<String>> {
    let mut out = vec![String::new()];
    for c in s.chars() {
        let up: String = c.to_uppercase().collect();
        let low: String = c.to_lowercase().collect();
        let forms: Vec<String> = if up != low { vec![low, up] } else { vec![c.to_string()] };
        out = out.iter().flat_map(|p| forms.iter().map(move |f| format!("{p}{f}"))).collect();
        if out.len() > 16 {
            return None;
        }
    }
    Some(out)
}

/// Name-index rows read for one term (three or more letters and digits)
/// before giving up on finding more exact matches.
const NAME_SCAN_BUDGET: usize = 100_000;
/// The same for one or two characters, which are in most names.
const SHORT_SCAN_BUDGET: usize = 20_000;

/// `(id, name, path)` of one file.
type NameRow = (i64, String, String);

fn name_row(r: &rusqlite::Row) -> rusqlite::Result<NameRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
}

fn push_new(seen: &mut std::collections::HashSet<i64>, out: &mut Vec<NameRow>, rows: Vec<NameRow>) {
    for r in rows {
        if seen.insert(r.0) {
            out.push(r);
        }
    }
}

pub fn source_code(s: TextSource) -> i64 {
    match s {
        TextSource::TextLayer => 1,
        TextSource::Ocr => 2,
        TextSource::Verified => 3,
        _ => 0,
    }
}

pub fn source_of(code: i64) -> TextSource {
    match code {
        1 => TextSource::TextLayer,
        2 => TextSource::Ocr,
        3 => TextSource::Verified,
        _ => TextSource::FileName,
    }
}

/// How a file's name (or path, for a term with `\`) answers one term: the
/// candidate, and whether the whole name is the text typed. Names are exact
/// text, so look-alike folding never applies.
pub fn name_candidate(term: &Term, file_id: i64, name: &str, path: &str) -> Option<(bool, Candidate)> {
    let d = term.explain_name(name, path)?;
    let whole = term.is_whole_name(name, path);
    let why = if whole {
        if term.is_path() { "This is the path typed".to_string() } else { "The file name is the text typed".to_string() }
    } else {
        format!("In the file {}: {}", if term.is_path() { "path" } else { "name" }, d.why.to_lowercase())
    };
    Some((whole, Candidate { file_id, page: None, text: name.to_string(), conf: 1.0, source: TextSource::FileName, bbox: None, class: d.class, why }))
}

/// A file where every term is in the name or somewhere in its folder path
/// (`ISO 2632 HOLD`, `Rev-1 PSV-2809`).
pub fn all_terms_candidate(terms: &[Term], file_id: i64, name: &str, path: &str) -> Option<Candidate> {
    let dir = path.rsplit_once('\\').map(|(d, _)| d).unwrap_or("");
    let mut found: Vec<MatchDetail> = Vec::new();
    for t in terms {
        let d = t.explain_name(name, path).or_else(|| {
            if t.is_path() {
                return None;
            }
            let d = if t.has_wildcards() {
                glob_match(&format!("*{}*", t.raw), &dir.to_lowercase()).then(|| MatchDetail { class: MatchClass::Partial, why: "Part of the folder path matches the pattern".into() })
            } else {
                t.explain(dir, 1.0).filter(|d| d.class != MatchClass::Uncertain)
            };
            d.map(|d| MatchDetail { class: d.class, why: format!("In the folder path: {}", d.why.to_lowercase()) })
        });
        found.push(d?);
    }
    let class = found.iter().map(|d| d.class).max().unwrap_or(MatchClass::Partial);
    let why = found.iter().map(|d| d.why.clone()).collect::<Vec<_>>().join("; ");
    Some(Candidate { file_id, page: None, text: name.to_string(), conf: 1.0, source: TextSource::FileName, bbox: None, class, why: format!("Every word is in the file name or path: {}", why.to_lowercase()) })
}

/// Quote a string for an FTS5 phrase query.
fn fts_phrase(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn bbox_of(r: &rusqlite::Row, i: usize) -> rusqlite::Result<NormBox> {
    Ok(NormBox { x: r.get::<_, f64>(i)? as f32, y: r.get::<_, f64>(i + 1)? as f32, w: r.get::<_, f64>(i + 2)? as f32, h: r.get::<_, f64>(i + 3)? as f32 })
}

fn file_row(r: &rusqlite::Row) -> rusqlite::Result<FileRow> {
    Ok(FileRow { id: r.get(0)?, path: r.get(1)?, name: r.get(2)?, size: r.get::<_, i64>(3)? as u64, mtime: r.get(4)?, read_state: r.get(5)? })
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open index {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=15000; PRAGMA journal_size_limit=67108864;")?;
        let mut s = Store { conn };
        s.migrate(Some(path))?;
        Ok(s)
    }

    /// Open a published part. Read-only and immutable: SQLite takes no locks
    /// and trusts that the file never changes, which is exactly what a part
    /// is and what makes reading it from a network share safe. Readings that
    /// were replaced since can be hidden ([`Store::set_hidden`]) without
    /// touching the file: a temporary view stands in for `files` in every
    /// query. A part made by a newer Sleuth is refused.
    pub fn open_part(path: &Path) -> Result<Store> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(sqlite_uri(path, "immutable=1&mode=ro"), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("open part {}", path.display()))?;
        conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA cache_size=-32768;")?;
        let s = Store { conn };
        let level: i64 = s.get_meta("schema")?.and_then(|v| v.parse().ok()).unwrap_or(1);
        if level > SCHEMA_LEVEL {
            anyhow::bail!("{} was made by a newer Sleuth (level {level}); update Sleuth on this PC to search it", path.display());
        }
        if level < 5 {
            anyhow::bail!("{} is not a Sleuth 1.1 part (level {level})", path.display());
        }
        s.conn.execute_batch(
            "CREATE TEMP TABLE hidden(id INTEGER PRIMARY KEY);
             CREATE TEMP VIEW files AS SELECT * FROM main.files WHERE id NOT IN (SELECT id FROM temp.hidden);",
        )?;
        // Parts written by 1.1.0 hold title-block details read from emails
        // and text files, which have no title block of their own
        // (`FileKind::has_title_block`); they are left out here.
        s.conn.execute_batch(&format!(
            "CREATE TEMP VIEW drawing_metadata AS SELECT * FROM main.drawing_metadata m
             WHERE NOT EXISTS (SELECT 1 FROM main.files f WHERE f.id=m.file_id AND ({}));",
            NO_TITLE_BLOCK_SQL
        ))?;
        Ok(s)
    }

    /// A new database written in one go and then published (a merged part):
    /// no journal and no syncing, because a half-written one is thrown away.
    pub fn create_bulk(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).with_context(|| format!("create {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA locking_mode=EXCLUSIVE; PRAGMA foreign_keys=ON; PRAGMA cache_size=-131072; PRAGMA temp_store=MEMORY;")?;
        let mut s = Store { conn };
        s.migrate(None)?;
        Ok(s)
    }

    /// Hide these readings of a part from every query (see [`Store::open_part`]).
    pub fn set_hidden(&self, ids: &[i64]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM temp.hidden", [])?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO temp.hidden(id) VALUES(?1)")?;
            for id in ids {
                ins.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every reading held (hidden ones included): what this part covers.
    pub fn reading_rows(&self) -> Result<Vec<ReadingRow>> {
        let mut st = self.conn.prepare("SELECT id, path, size, mtime, read_state, COALESCE(read_at, 0), COALESCE(read_error, '') FROM main.files WHERE read_state IN (?1, ?2, ?3)")?;
        let rows = st.query_map(params![state::READ, state::FAILED, state::NEEDS_OCR], |r| {
            Ok(ReadingRow { id: r.get(0)?, path: r.get(1)?, size: r.get::<_, i64>(2)? as u64, mtime: r.get(3)?, state: r.get(4)?, read_at: r.get(5)?, error: r.get(6)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Copy one reading (or recorded failure) from another store, with the
    /// corrections people made to it. Tags and title-block details are
    /// derived again here.
    pub fn import_reading(&mut self, src: &Store, src_id: i64, deriver: &Deriver) -> Result<i64> {
        self.import_reading_as(src, src_id, None, deriver)
    }

    /// As [`Store::import_reading`], stored under another path (an index made
    /// before 1.1 wrote shares by name; the office store writes addresses).
    pub fn import_reading_as(&mut self, src: &Store, src_id: i64, path_as: Option<&str>, deriver: &Deriver) -> Result<i64> {
        #[allow(clippy::type_complexity)]
        let (path, dir, name, size, mtime, st, error, hash, read_at): (String, String, String, i64, i64, i64, Option<String>, Option<String>, Option<i64>) = src.conn.query_row(
            "SELECT path, dir, name, size, mtime, read_state, read_error, hash, read_at FROM main.files WHERE id=?1",
            [src_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
        )?;
        let (path, dir) = match path_as {
            Some(p) => (p.to_string(), p.rsplit_once('\\').map(|(d, _)| if d.len() == 2 { format!("{d}\\") } else { d.to_string() }).unwrap_or(dir)),
            None => (path, dir),
        };
        let (id, _) = self.upsert_file(&path, &dir, &name, size as u64, mtime, 0, state::WAITING)?;
        let read_at = read_at.unwrap_or(0);
        if st == state::NEEDS_OCR {
            self.save_needs_ocr(id, read_at)?;
            return Ok(id);
        }
        if st != state::READ {
            self.save_failure(id, error.as_deref().unwrap_or("could not be read"), read_at)?;
            return Ok(id);
        }
        let text = src.file_text(src_id)?;
        let hash = hash.unwrap_or_default();
        {
            let mut st = src.conn.prepare("SELECT file_hash, page, x, y, w, h, original, original_conf, corrected, user, at, stale FROM corrections WHERE file_id=?1 ORDER BY id")?;
            let mut rows = st.query([src_id])?;
            self.conn.execute("DELETE FROM corrections WHERE file_id=?1", [id])?;
            while let Some(r) = rows.next()? {
                self.conn.execute(
                    "INSERT INTO corrections(file_id, file_hash, page, x, y, w, h, original, original_conf, corrected, user, at, stale) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![
                        id,
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<f64>>(2)?,
                        r.get::<_, Option<f64>>(3)?,
                        r.get::<_, Option<f64>>(4)?,
                        r.get::<_, Option<f64>>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, f64>(7)?,
                        r.get::<_, String>(8)?,
                        r.get::<_, String>(9)?,
                        r.get::<_, i64>(10)?,
                        r.get::<_, i64>(11)?
                    ],
                )?;
            }
        }
        self.save_read(id, &hash, &text, read_at, deriver)?;
        Ok(id)
    }

    /// Forget every reading (after they were published as a part).
    pub fn clear_readings(&self) -> Result<()> {
        self.conn.execute_batch(
            "BEGIN IMMEDIATE;
             DELETE FROM files;
             DELETE FROM engineering_tags;
             DELETE FROM dirs;
             INSERT INTO lines_fts(lines_fts) VALUES('delete-all');
             INSERT INTO names(names) VALUES('delete-all');
             COMMIT;",
        )?;
        // Emptied pages stay in the file until it is rebuilt; a PC is
        // meant to keep only a few MB.
        let pages: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap_or(0);
        let size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap_or(4096);
        if pages * size > 16 * 1024 * 1024 {
            let _ = self.conn.execute_batch("VACUUM;");
        }
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        Ok(())
    }

    /// Readings held: (read, could not be read or waiting for OCR).
    pub fn reading_counts(&self) -> Result<(u64, u64)> {
        let n = |s: i64| -> Result<u64> { Ok(self.conn.query_row("SELECT COUNT(*) FROM main.files WHERE read_state=?1", [s], |r| r.get::<_, i64>(0))? as u64) };
        Ok((n(state::READ)?, n(state::FAILED)? + n(state::NEEDS_OCR)?))
    }

    /// Record that a file was opened and needs OCR ([`state::NEEDS_OCR`]).
    pub fn save_needs_ocr(&self, file_id: i64, now: i64) -> Result<()> {
        self.clear_lines(file_id)?;
        self.conn.execute("UPDATE files SET read_state=?2, read_error=NULL, read_at=?3, priority=0, lease_until=NULL WHERE id=?1", params![file_id, state::NEEDS_OCR, now])?;
        Ok(())
    }

    /// `meta` values whose key starts with `prefix`, keyed by the rest.
    pub fn meta_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT key, value FROM meta WHERE key LIKE ?1")?;
        let rows = st.query_map([format!("{prefix}%")], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().filter_map(|(k, v)| k.strip_prefix(prefix).map(|k| (k.to_string(), v))).collect())
    }

    /// Files by id in one query per 500 (hidden ones are left out).
    pub fn files_by_ids(&self, ids: &[i64]) -> Result<HashMap<i64, FileRow>> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut st = self.conn.prepare(&format!("SELECT id, path, name, size, mtime, read_state FROM files WHERE id IN ({marks})"))?;
            for row in st.query_map(params_from_iter(chunk.iter()), file_row)? {
                let f = row?;
                out.insert(f.id, f);
            }
        }
        Ok(out)
    }

    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let mut s = Store { conn };
        s.migrate(None)?;
        Ok(s)
    }

    fn migrate(&mut self, path: Option<&Path>) -> Result<()> {
        let v: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v > SCHEMA_VERSION {
            anyhow::bail!("index was made by a newer Sleuth; it will be rebuilt");
        }
        if v < 1 {
            self.conn.execute_batch(
                "BEGIN;
                CREATE TABLE IF NOT EXISTS dirs(
                    path TEXT PRIMARY KEY COLLATE NOCASE,
                    parent TEXT COLLATE NOCASE,
                    seen INTEGER NOT NULL);
                CREATE INDEX IF NOT EXISTS dirs_parent ON dirs(parent);
                CREATE TABLE IF NOT EXISTS files(
                    id INTEGER PRIMARY KEY,
                    path TEXT NOT NULL UNIQUE COLLATE NOCASE,
                    dir TEXT NOT NULL COLLATE NOCASE,
                    name TEXT NOT NULL,
                    size INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    seen INTEGER NOT NULL,
                    read_state INTEGER NOT NULL DEFAULT 0,
                    read_error TEXT,
                    hash TEXT,
                    pages INTEGER,
                    model TEXT,
                    read_at INTEGER,
                    priority INTEGER NOT NULL DEFAULT 0,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    retry_at INTEGER);
                CREATE INDEX IF NOT EXISTS files_dir ON files(dir);
                CREATE INDEX IF NOT EXISTS files_queue ON files(read_state, priority DESC, mtime DESC);
                CREATE INDEX IF NOT EXISTS files_hash ON files(hash);
                CREATE VIRTUAL TABLE IF NOT EXISTS names USING fts5(fold, tokenize='trigram', content='', contentless_delete=1);
                CREATE TABLE IF NOT EXISTS lines(
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    page INTEGER NOT NULL,
                    text TEXT NOT NULL,
                    conf REAL NOT NULL,
                    source INTEGER NOT NULL,
                    alt INTEGER NOT NULL DEFAULT 0,
                    x REAL, y REAL, w REAL, h REAL);
                CREATE INDEX IF NOT EXISTS lines_file ON lines(file_id, page);
                CREATE VIRTUAL TABLE IF NOT EXISTS lines_fts USING fts5(fold, tokenize='trigram', content='', contentless_delete=1);
                CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
                PRAGMA user_version=1;
                COMMIT;",
            )?;
        }
        let level: i64 = self.get_meta("schema")?.and_then(|v| v.parse().ok()).unwrap_or(1);
        if level < 2 {
            // Keep a copy of the level-1 index before changing it.
            if let Some(p) = path
                && v >= 1
            {
                let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                let bak: PathBuf = p.with_extension("v1.bak.db");
                if !bak.exists() {
                    let _ = std::fs::copy(p, &bak);
                }
            }
            self.upgrade_to_2()?;
        }
        if level < 3 {
            self.upgrade_to_3()?;
        }
        if level < 4 {
            self.upgrade_to_4()?;
        }
        if level < 5 {
            self.upgrade_to_5()?;
        }
        if level > SCHEMA_LEVEL {
            anyhow::bail!("index was made by a newer Sleuth; it will be rebuilt");
        }
        Ok(())
    }

    /// Level 5 (Sleuth 1.1): the size of each page, which tells a drawing
    /// sheet from a document page.
    fn upgrade_to_5(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let cols: Vec<String> = {
            let mut st = tx.prepare("PRAGMA table_info(pages)")?;
            st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<_>>()?
        };
        for col in ["w_pt", "h_pt"] {
            if !cols.iter().any(|c| c == col) {
                tx.execute_batch(&format!("ALTER TABLE pages ADD COLUMN {col} REAL NOT NULL DEFAULT 0;"))?;
            }
        }
        tx.execute_batch("INSERT INTO meta(key, value) VALUES('schema', '5') ON CONFLICT(key) DO UPDATE SET value='5';")?;
        tx.commit()?;
        Ok(())
    }

    /// Level 4: an index on file names, so a file named exactly as typed is
    /// found (and ranked first) however many other names contain the text.
    fn upgrade_to_4(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "BEGIN;
            CREATE INDEX IF NOT EXISTS files_name ON files(name COLLATE NOCASE);
            INSERT INTO meta(key, value) VALUES('schema', '4') ON CONFLICT(key) DO UPDATE SET value='4';
            COMMIT;",
        )?;
        Ok(())
    }

    /// Level 3: `dirs.listed` records the walk that actually listed a folder.
    /// Before it, `seen` was also set on subfolders when their parent was
    /// listed, so a resumed or racing walk took them as listed and skipped
    /// their contents. Existing folders count as never listed, and the next
    /// walk starts afresh.
    fn upgrade_to_3(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let has: bool = {
            let mut st = tx.prepare("PRAGMA table_info(dirs)")?;
            let cols: Vec<String> = st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<_>>()?;
            cols.iter().any(|c| c == "listed")
        };
        if !has {
            tx.execute_batch("ALTER TABLE dirs ADD COLUMN listed INTEGER;")?;
        }
        tx.execute_batch(
            "DELETE FROM meta WHERE key='pass';
            INSERT INTO meta(key, value) VALUES('schema', '3') ON CONFLICT(key) DO UPDATE SET value='3';",
        )?;
        tx.commit()?;
        Ok(())
    }

    fn upgrade_to_2(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let cols: Vec<String> = {
            let mut st = tx.prepare("PRAGMA table_info(files)")?;
            st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<_>>()?
        };
        for (col, ty) in [("lease_until", "INTEGER"), ("lease_owner", "TEXT"), ("pipeline", "TEXT"), ("derived", "TEXT"), ("gone_at", "INTEGER"), ("provenance", "TEXT")] {
            if !cols.iter().any(|c| c == col) {
                tx.execute_batch(&format!("ALTER TABLE files ADD COLUMN {col} {ty};"))?;
            }
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS pages(
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                page INTEGER NOT NULL,
                class TEXT NOT NULL,
                chars INTEGER NOT NULL,
                image_coverage REAL NOT NULL,
                ocr_tiles INTEGER NOT NULL,
                ocr_skipped INTEGER NOT NULL,
                ocr_ms INTEGER NOT NULL,
                note TEXT NOT NULL,
                PRIMARY KEY(file_id, page));
            CREATE TABLE IF NOT EXISTS engineering_tags(
                id INTEGER PRIMARY KEY,
                norm TEXT NOT NULL UNIQUE,
                category TEXT NOT NULL,
                fold TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS tags_fold ON engineering_tags(fold);
            CREATE INDEX IF NOT EXISTS tags_category ON engineering_tags(category);
            CREATE TABLE IF NOT EXISTS tag_occurrences(
                id INTEGER PRIMARY KEY,
                tag_id INTEGER NOT NULL REFERENCES engineering_tags(id) ON DELETE CASCADE,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                page INTEGER NOT NULL,
                raw TEXT NOT NULL,
                conf REAL NOT NULL,
                source INTEGER NOT NULL,
                x REAL, y REAL, w REAL, h REAL,
                parser TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS occ_tag ON tag_occurrences(tag_id);
            CREATE INDEX IF NOT EXISTS occ_file ON tag_occurrences(file_id);
            CREATE TABLE IF NOT EXISTS drawing_metadata(
                file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
                drawing_no TEXT, drawing_norm TEXT,
                title TEXT, revision TEXT, revision_norm TEXT,
                project TEXT, discipline TEXT, sheet TEXT, status TEXT,
                evidence TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS meta_dwg ON drawing_metadata(drawing_norm);
            CREATE TABLE IF NOT EXISTS corrections(
                id INTEGER PRIMARY KEY,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                file_hash TEXT NOT NULL,
                page INTEGER NOT NULL,
                x REAL, y REAL, w REAL, h REAL,
                original TEXT NOT NULL,
                original_conf REAL NOT NULL,
                corrected TEXT NOT NULL,
                user TEXT NOT NULL,
                at INTEGER NOT NULL,
                stale INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS corr_file ON corrections(file_id);
            CREATE TABLE IF NOT EXISTS projects(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE COLLATE NOCASE);
            CREATE TABLE IF NOT EXISTS project_roots(
                project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                root TEXT NOT NULL COLLATE NOCASE);
            CREATE TABLE IF NOT EXISTS volume_checkpoints(
                volume TEXT PRIMARY KEY,
                journal_id TEXT NOT NULL,
                next_usn INTEGER NOT NULL,
                updated INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS index_events(id INTEGER PRIMARY KEY, at INTEGER NOT NULL, level TEXT NOT NULL, message TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS search_history(query TEXT PRIMARY KEY, at INTEGER NOT NULL, pinned INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS root_scans(root TEXT PRIMARY KEY COLLATE NOCASE, state TEXT NOT NULL, last_ok INTEGER NOT NULL DEFAULT 0, note TEXT NOT NULL DEFAULT '');
            INSERT INTO meta(key, value) VALUES('schema', '2') ON CONFLICT(key) DO UPDATE SET value='2';",
        )?;
        tx.commit()?;
        Ok(())
    }

    // ---- file names -------------------------------------------------------

    /// Insert or refresh one file seen by a walk. `wanted` is the state a new
    /// or changed file gets ([`state::NOT_PDF`], `WAITING`, `UNREAD` or
    /// `OFFICE`). Returns `(id, changed)`; `changed` is true for new files and
    /// files whose size or date moved.
    pub fn upsert_file(&self, path: &str, dir: &str, name: &str, size: u64, mtime: i64, seen: i64, wanted: i64) -> Result<(i64, bool)> {
        let existing: Option<(i64, i64, i64, i64)> = self
            .conn
            .query_row("SELECT id, size, mtime, read_state FROM files WHERE path=?1", [path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?;
        let open = |s: i64| matches!(s, state::WAITING | state::UNREAD | state::OFFICE);
        match existing {
            Some((id, osize, omtime, ostate)) => {
                let changed = osize as u64 != size || omtime != mtime;
                let new_state = if changed {
                    if matches!(ostate, state::READ | state::FAILED | state::GONE) {
                        self.clear_lines(id)?;
                    }
                    wanted
                } else if ostate == state::GONE {
                    // Back where it was, unchanged: its readings are still here.
                    if self.conn.query_row("SELECT hash IS NOT NULL FROM files WHERE id=?1", [id], |r| r.get::<_, bool>(0))? { state::READ } else { wanted }
                } else if open(ostate) && open(wanted) {
                    wanted
                } else {
                    ostate
                };
                if ostate == state::GONE {
                    self.conn.execute("INSERT INTO names(rowid, fold) VALUES(?1, ?2)", params![id, fold_form(name)])?;
                }
                self.conn.execute(
                    "UPDATE files SET size=?2, mtime=?3, seen=?4, read_state=?5, gone_at=NULL, dir=?6, name=?7 WHERE id=?1",
                    params![id, size as i64, mtime, seen, new_state, dir, name],
                )?;
                Ok((id, changed))
            }
            None => {
                self.conn.execute(
                    "INSERT INTO files(path, dir, name, size, mtime, seen, read_state) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![path, dir, name, size as i64, mtime, seen, wanted],
                )?;
                let id = self.conn.last_insert_rowid();
                self.conn.execute("INSERT INTO names(rowid, fold) VALUES(?1, ?2)", params![id, fold_form(name)])?;
                Ok((id, true))
            }
        }
    }

    /// Record a folder seen in pass `seen` (as a subfolder of a listing).
    pub fn touch_dir(&self, path: &str, parent: Option<&str>, seen: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO dirs(path, parent, seen) VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET seen=excluded.seen",
            params![path, parent, seen],
        )?;
        Ok(())
    }

    /// Record that a folder itself was listed completely in pass `seen`.
    pub fn mark_listed(&self, path: &str, parent: Option<&str>, seen: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO dirs(path, parent, seen, listed) VALUES(?1,?2,?3,?3) ON CONFLICT(path) DO UPDATE SET seen=excluded.seen, listed=excluded.listed",
            params![path, parent, seen],
        )?;
        Ok(())
    }

    /// Subfolders of a folder already listed in pass `seen` (resuming an
    /// interrupted walk); `None` when the folder was not listed in it.
    pub fn listed_in_pass(&self, dir: &str, seen: i64) -> Result<Option<Vec<String>>> {
        let done: Option<Option<i64>> = self.conn.query_row("SELECT listed FROM dirs WHERE path=?1", [dir], |r| r.get(0)).optional()?;
        if done.flatten() != Some(seen) {
            return Ok(None);
        }
        let mut st = self.conn.prepare_cached("SELECT path FROM dirs WHERE parent=?1")?;
        let kids = st.query_map([dir], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(Some(kids))
    }

    /// After a folder was listed completely: forget its files and subfolders
    /// that were not seen in this listing. Never called for a listing that
    /// failed or timed out, so an unreachable folder never loses its files.
    /// Files the index holds directly in `dir`, by lower-case name:
    /// `(id, size, mtime, read_state)`.
    pub fn dir_files(&self, dir: &str) -> Result<HashMap<String, (i64, u64, i64, i64)>> {
        let mut st = self.conn.prepare_cached("SELECT id, name, size, mtime, read_state FROM files WHERE dir=?1")?;
        let rows = st.query_map([dir], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, name, size, mtime, st) = row?;
            out.insert(name.to_lowercase(), (id, size, mtime, st));
        }
        Ok(out)
    }

    /// Subfolders the index holds for `dir`, lower case.
    pub fn dir_children(&self, dir: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT path FROM dirs WHERE parent=?1")?;
        let rows = st.query_map([dir], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let p = row?;
            out.push((p.to_lowercase(), p));
        }
        Ok(out)
    }

    pub fn reconcile_dir(&self, dir: &str, seen: i64, now: i64) -> Result<()> {
        let gone: Vec<i64> = {
            let mut st = self.conn.prepare_cached("SELECT id FROM files WHERE dir=?1 AND seen<?2 AND read_state<>?3")?;
            st.query_map(params![dir, seen, state::GONE], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in gone {
            self.delete_file(id, now)?;
        }
        let gone_dirs: Vec<String> = {
            let mut st = self.conn.prepare_cached("SELECT path FROM dirs WHERE parent=?1 AND seen<?2")?;
            st.query_map(params![dir, seen], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for d in gone_dirs {
            self.delete_tree(&d, now)?;
        }
        Ok(())
    }

    /// Forget a folder and everything under it, as if deleted (readings are
    /// kept for the grace period, for identical copies elsewhere).
    pub fn forget_tree(&self, dir: &str, now: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.delete_tree(dir, now)?;
        let d = dir.trim_end_matches('\\');
        self.conn.execute("DELETE FROM root_scans WHERE root=?1 OR root=?2", params![d, format!("{d}\\")])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn delete_tree(&self, dir: &str, now: i64) -> Result<()> {
        // Range scans of the folder indexes (`\` sorts just before `]`): a
        // LIKE here read every file of the index, and ran whenever a folder
        // disappeared during a walk.
        let d = dir.trim_end_matches('\\');
        let (lo, hi) = (format!("{d}\\"), format!("{d}]"));
        let ids: Vec<i64> = {
            let mut st = self.conn.prepare_cached(
                "SELECT id FROM files WHERE dir=?1 AND read_state<>?4 UNION ALL SELECT id FROM files WHERE dir>=?2 AND dir<?3 AND read_state<>?4",
            )?;
            st.query_map(params![d, lo, hi, state::GONE], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            self.delete_file(id, now)?;
        }
        self.conn.execute("DELETE FROM dirs WHERE path=?1", params![d])?;
        self.conn.execute("DELETE FROM dirs WHERE path>=?1 AND path<?2", params![lo, hi])?;
        Ok(())
    }

    /// A file is no longer where it was. A read file keeps its readings for a
    /// while (state `GONE`) so the same content found elsewhere is not read
    /// again; anything else is removed now.
    pub(crate) fn delete_file(&self, id: i64, now: i64) -> Result<()> {
        let (st, has_hash): (i64, bool) = self.conn.query_row("SELECT read_state, hash IS NOT NULL FROM files WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        self.conn.execute("DELETE FROM names WHERE rowid=?1", [id])?;
        if st == state::READ && has_hash {
            self.conn.execute("UPDATE files SET read_state=?2, gone_at=?3, lease_until=NULL WHERE id=?1", params![id, state::GONE, now])?;
            return Ok(());
        }
        self.clear_lines(id)?;
        self.conn.execute("DELETE FROM files WHERE id=?1", [id])?;
        Ok(())
    }

    /// Forget removed files whose grace period is over.
    pub fn purge_gone(&self, now: i64) -> Result<usize> {
        let ids: Vec<i64> = {
            let mut st = self.conn.prepare("SELECT id FROM files WHERE read_state=?1 AND gone_at<?2")?;
            st.query_map(params![state::GONE, now - GONE_KEEP_SECS], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in &ids {
            self.clear_lines(*id)?;
            self.conn.execute("DELETE FROM files WHERE id=?1", [id])?;
        }
        Ok(ids.len())
    }

    fn clear_lines(&self, file_id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM lines_fts WHERE rowid IN (SELECT id FROM lines WHERE file_id=?1)", [file_id])?;
        self.conn.execute("DELETE FROM lines WHERE file_id=?1", [file_id])?;
        self.conn.execute("DELETE FROM tag_occurrences WHERE file_id=?1", [file_id])?;
        self.conn.execute("DELETE FROM drawing_metadata WHERE file_id=?1", [file_id])?;
        self.conn.execute("DELETE FROM pages WHERE file_id=?1", [file_id])?;
        self.conn.execute("UPDATE corrections SET stale=1 WHERE file_id=?1", [file_id])?;
        Ok(())
    }

    // ---- reading queue ----------------------------------------------------

    /// Take the next PDF to read: requested ones first, then newest. The job
    /// is leased to `owner`; a worker that stops without finishing leaves a
    /// lease that expires, and the file is picked up again.
    pub fn next_to_read(&self, now: i64) -> Result<Option<FileRow>> {
        self.claim_next(now, "local")
    }

    pub fn claim_next(&self, now: i64, owner: &str) -> Result<Option<FileRow>> {
        self.claim_from(now, owner, 0)
    }

    /// Claim only files someone asked for (search, open), not the backlog.
    pub fn claim_urgent(&self, now: i64, owner: &str) -> Result<Option<FileRow>> {
        self.claim_from(now, owner, 1)
    }

    fn claim_from(&self, now: i64, owner: &str, min_priority: i64) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row(
                "UPDATE files SET lease_until=?3, lease_owner=?4 WHERE id=(
                    SELECT id FROM files WHERE read_state=?1 AND priority>=?5 AND (retry_at IS NULL OR retry_at<=?2) AND (lease_until IS NULL OR lease_until<=?2)
                    ORDER BY priority DESC, mtime DESC LIMIT 1)
                 RETURNING id, path, name, size, mtime, read_state",
                params![state::WAITING, now, now + LEASE_SECS, owner, min_priority],
                file_row,
            )
            .optional()?)
    }

    /// PDFs under one reading folder by state: (all, read, waiting, failed).
    pub fn scope_counts(&self, folder: &str) -> Result<(u64, u64, u64, u64)> {
        let f = folder.trim_end_matches('\\');
        // Two range scans of the folder index (`\` sorts just before `]`),
        // not a LIKE over every file: this runs on indexes of millions.
        let mut st = self.conn.prepare_cached(
            "SELECT read_state, COUNT(*) FROM (
                SELECT read_state FROM files WHERE dir=?1
                UNION ALL SELECT read_state FROM files WHERE dir>=?2 AND dir<?8)
             WHERE read_state IN (?3,?4,?5,?6,?7) GROUP BY read_state",
        )?;
        let (mut all, mut read, mut waiting, mut failed) = (0u64, 0u64, 0u64, 0u64);
        let rows = st.query_map(
            params![f, format!("{f}\\"), state::WAITING, state::READ, state::FAILED, state::UNREAD, state::OFFICE, format!("{f}]")],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64)),
        )?;
        for row in rows {
            let (s, n) = row?;
            all += n;
            match s {
                state::READ => read += n,
                state::WAITING => waiting += n,
                state::FAILED => failed += n,
                _ => {}
            }
        }
        Ok((all, read, waiting, failed))
    }

    /// Files in one read state.
    pub fn count_state(&self, s: i64) -> Result<u64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM files WHERE read_state=?1", [s], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Give a job back without a result (reader stopped).
    pub fn release(&self, file_id: i64) -> Result<()> {
        self.conn.execute("UPDATE files SET lease_until=NULL, lease_owner=NULL WHERE id=?1", [file_id])?;
        Ok(())
    }

    /// Keep a long job's lease alive.
    pub fn renew_lease(&self, file_id: i64, now: i64) -> Result<()> {
        self.conn.execute("UPDATE files SET lease_until=?2 WHERE id=?1", params![file_id, now + LEASE_SECS])?;
        Ok(())
    }

    /// Queue a file for reading ahead of everything else.
    pub fn request_read(&self, path: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE files SET read_state=?2, priority=1, attempts=0, retry_at=NULL WHERE path=?1 AND read_state IN (?3, ?4, ?2, ?5)",
            params![path, state::WAITING, state::UNREAD, state::FAILED, state::OFFICE],
        )?;
        Ok(n > 0)
    }

    pub fn file_by_path(&self, path: &str) -> Result<Option<FileRow>> {
        Ok(self.conn.query_row("SELECT id, path, name, size, mtime, read_state FROM files WHERE path=?1", [path], file_row).optional()?)
    }

    /// Title-block details of many files in one query per 500.
    pub fn metadata_many(&self, ids: &[i64]) -> Result<HashMap<i64, DrawingMeta>> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!("SELECT file_id, drawing_no, title, revision, project, discipline, sheet, status, evidence FROM drawing_metadata WHERE file_id IN ({marks})");
            let mut st = self.conn.prepare(&sql)?;
            let rows = st.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    DrawingMeta {
                        drawing_no: r.get(1)?,
                        title: r.get(2)?,
                        revision: r.get(3)?,
                        project: r.get(4)?,
                        discipline: r.get(5)?,
                        sheet: r.get(6)?,
                        status: r.get(7)?,
                        evidence: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
                    },
                ))
            })?;
            for row in rows {
                let (id, m) = row?;
                out.insert(id, m);
            }
        }
        Ok(out)
    }

    /// Which of these files have a page of drawing size (long side at
    /// least `min_side_pt` points; A3 is 1190).
    pub fn drawing_sized(&self, ids: &[i64], min_side_pt: f32) -> Result<std::collections::HashSet<i64>> {
        let mut out = std::collections::HashSet::new();
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut st = self.conn.prepare(&format!("SELECT DISTINCT file_id FROM pages WHERE file_id IN ({marks}) AND MAX(w_pt, h_pt) >= {min_side_pt}"))?;
            for row in st.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, i64>(0))? {
                out.insert(row?);
            }
        }
        Ok(out)
    }

    /// Read documents whose title block gives one of these drawing numbers
    /// (normalised): `(drawing_norm, revision_norm, path, name)`.
    pub fn docs_by_drawing(&self, norms: &[String]) -> Result<Vec<(String, Option<String>, String, String)>> {
        let mut out = Vec::new();
        for chunk in norms.chunks(300) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut st = self.conn.prepare(&format!(
                "SELECT m.drawing_norm, m.revision_norm, f.path, f.name FROM drawing_metadata m JOIN files f ON f.id=m.file_id WHERE m.drawing_norm IN ({marks}) AND f.read_state={} LIMIT 5000",
                state::READ
            ))?;
            for row in st.query_map(params_from_iter(chunk.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))? {
                out.push(row?);
            }
        }
        Ok(out)
    }

    pub fn file_by_id(&self, id: i64) -> Result<Option<FileRow>> {
        Ok(self.conn.prepare_cached("SELECT id, path, name, size, mtime, read_state FROM files WHERE id=?1")?.query_row([id], file_row).optional()?)
    }

    /// A file already read with the same content (copied, moved or renamed).
    pub fn read_copy_by_hash(&self, hash: &str, except: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM files WHERE hash=?1 AND read_state IN (?2, ?3) AND id<>?4 ORDER BY read_state LIMIT 1",
                params![hash, state::READ, state::GONE, except],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Lines, page classes and provenance of a read file (without
    /// corrections, which belong to the file, not the reading).
    pub fn file_text(&self, file_id: i64) -> Result<FileText> {
        let mut st = self.conn.prepare("SELECT page, text, conf, source, alt, x, y, w, h FROM lines WHERE file_id=?1 AND source<>3 ORDER BY page, id")?;
        let mut ft = FileText::default();
        let rows = st.query_map([file_id], |r| {
            Ok((r.get::<_, u32>(0)?, Line { text: r.get(1)?, conf: r.get::<_, f64>(2)? as f32, source: source_of(r.get(3)?), alt: r.get::<_, i64>(4)? != 0, bbox: bbox_of(r, 5)? }))
        })?;
        for row in rows {
            let (page, line) = row?;
            if ft.pages.last().is_none_or(|p| p.page != page) {
                ft.pages.push(crate::text::PageText { page, lines: Vec::new() });
            }
            ft.pages.last_mut().unwrap().lines.push(line);
        }
        ft.page_info = self.page_info(file_id)?;
        let (model, prov): (Option<String>, Option<String>) = self.conn.query_row("SELECT model, provenance FROM files WHERE id=?1", [file_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        ft.model = model.unwrap_or_default();
        ft.provenance = prov.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default();
        ft.reader_version = crate::text::READER_VERSION;
        Ok(ft)
    }

    pub fn page_info(&self, file_id: i64) -> Result<Vec<PageInfo>> {
        let mut st = self.conn.prepare_cached("SELECT page, class, chars, image_coverage, ocr_tiles, ocr_skipped, ocr_ms, note, w_pt, h_pt FROM pages WHERE file_id=?1 ORDER BY page")?;
        let rows = st.query_map([file_id], |r| {
            Ok(PageInfo { page: r.get(0)?, class: r.get(1)?, chars: r.get(2)?, image_coverage: r.get::<_, f64>(3)? as f32, ocr_tiles: r.get(4)?, ocr_tiles_skipped: r.get(5)?, ocr_millis: r.get::<_, i64>(6)? as u64, note: r.get(7)?, width_pt: r.get::<_, f64>(8)? as f32, height_pt: r.get::<_, f64>(9)? as f32 })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn provenance(&self, file_id: i64) -> Result<Provenance> {
        let p: Option<String> = self.conn.query_row("SELECT provenance FROM files WHERE id=?1", [file_id], |r| r.get(0)).optional()?.flatten();
        Ok(p.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default())
    }

    /// Store a reading: lines, page classes, provenance, then the tags and
    /// title-block details derived from it, and corrections made earlier to
    /// this same content.
    pub fn save_read(&mut self, file_id: i64, hash: &str, text: &FileText, now: i64, deriver: &Deriver) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM lines_fts WHERE rowid IN (SELECT id FROM lines WHERE file_id=?1)", [file_id])?;
        tx.execute("DELETE FROM lines WHERE file_id=?1", [file_id])?;
        tx.execute("DELETE FROM pages WHERE file_id=?1", [file_id])?;
        {
            let mut ins = tx.prepare_cached("INSERT INTO lines(file_id, page, text, conf, source, alt, x, y, w, h) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)")?;
            let mut fts = tx.prepare_cached("INSERT INTO lines_fts(rowid, fold) VALUES(?1, ?2)")?;
            for p in &text.pages {
                for l in &p.lines {
                    ins.execute(params![file_id, p.page, l.text, l.conf as f64, source_code(l.source), l.alt as i64, l.bbox.x as f64, l.bbox.y as f64, l.bbox.w as f64, l.bbox.h as f64])?;
                    fts.execute(params![tx.last_insert_rowid(), fold_form(&l.text)])?;
                }
            }
            let mut pg = tx.prepare_cached("INSERT OR REPLACE INTO pages(file_id, page, class, chars, image_coverage, ocr_tiles, ocr_skipped, ocr_ms, note, w_pt, h_pt) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?;
            for i in &text.page_info {
                pg.execute(params![file_id, i.page, i.class, i.chars, i.image_coverage as f64, i.ocr_tiles, i.ocr_tiles_skipped, i.ocr_millis as i64, i.note, i.width_pt as f64, i.height_pt as f64])?;
            }
            // Corrections recorded for this exact content apply again; for
            // other content they are kept but marked stale.
            tx.execute("UPDATE corrections SET stale=CASE WHEN file_hash=?2 THEN 0 ELSE 1 END WHERE file_id=?1", params![file_id, hash])?;
            let mut corr = tx.prepare_cached("SELECT page, corrected, x, y, w, h FROM corrections WHERE file_id=?1 AND stale=0")?;
            let rows: Vec<(u32, String, NormBox)> = corr.query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?, bbox_of(r, 2)?)))?.collect::<rusqlite::Result<_>>()?;
            for (page, t, b) in rows {
                ins.execute(params![file_id, page, t, 1.0f64, source_code(TextSource::Verified), 0i64, b.x as f64, b.y as f64, b.w as f64, b.h as f64])?;
                fts.execute(params![tx.last_insert_rowid(), fold_form(&t)])?;
            }
        }
        tx.execute(
            "UPDATE files SET read_state=?2, hash=?3, pages=?4, model=?5, read_at=?6, read_error=NULL, priority=0, attempts=0, retry_at=NULL,
                 lease_until=NULL, lease_owner=NULL, pipeline=?7, provenance=?8 WHERE id=?1",
            params![file_id, state::READ, hash, text.pages.len() as i64, text.model, now, text.provenance.pipeline, serde_json::to_string(&text.provenance)?],
        )?;
        let name: String = tx.query_row("SELECT name FROM files WHERE id=?1", [file_id], |r| r.get(0))?;
        derive_into(&tx, file_id, &name, deriver)?;
        tx.commit()?;
        Ok(())
    }

    /// Re-run the tag parser and title-block rules on a read file's stored
    /// lines (after the rules changed). No OCR is involved.
    pub fn rederive(&mut self, file_id: i64, deriver: &Deriver) -> Result<()> {
        let tx = self.conn.transaction()?;
        let name: String = tx.query_row("SELECT name FROM files WHERE id=?1", [file_id], |r| r.get(0))?;
        derive_into(&tx, file_id, &name, deriver)?;
        tx.commit()?;
        Ok(())
    }

    /// Read files whose derived data was made with other rules.
    pub fn stale_derived(&self, hash: &str, limit: usize) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare_cached("SELECT id FROM files WHERE read_state=?1 AND (derived IS NULL OR derived<>?2) LIMIT ?3")?;
        Ok(st.query_map(params![state::READ, hash, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// A temporary problem (network, file locked, another PC reading it):
    /// try again later, with a growing delay. After `max_attempts` the file
    /// is marked failed.
    pub fn defer(&self, file_id: i64, reason: &str, now: i64, max_attempts: i64) -> Result<bool> {
        let attempts: i64 = self.conn.query_row("SELECT attempts FROM files WHERE id=?1", [file_id], |r| r.get::<_, i64>(0))? + 1;
        if attempts >= max_attempts {
            self.save_failure(file_id, reason, now)?;
            return Ok(false);
        }
        let delay = 300 * (1i64 << attempts.min(6));
        self.conn.execute(
            "UPDATE files SET attempts=?2, retry_at=?3, read_error=?4, lease_until=NULL WHERE id=?1",
            params![file_id, attempts, now + delay, reason],
        )?;
        Ok(true)
    }

    /// Wait until `until` without counting an attempt (another PC holds the
    /// shared-cache claim for this file).
    pub fn wait_until(&self, file_id: i64, until: i64, reason: &str) -> Result<()> {
        self.conn.execute("UPDATE files SET retry_at=?2, read_error=?3, lease_until=NULL WHERE id=?1", params![file_id, until, reason])?;
        Ok(())
    }

    pub fn save_failure(&self, file_id: i64, reason: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET read_state=?2, read_error=?3, read_at=?4, priority=0, lease_until=NULL WHERE id=?1",
            params![file_id, state::FAILED, reason, now],
        )?;
        Ok(())
    }

    /// Put a file back in the queue (e.g. after the reader was stopped midway).
    pub fn requeue(&self, file_id: i64) -> Result<()> {
        self.conn.execute("UPDATE files SET read_state=?2, lease_until=NULL WHERE id=?1", params![file_id, state::WAITING])?;
        Ok(())
    }

    // ---- search -----------------------------------------------------------

    /// Matching lines for one term inside `scope`, best first, at most `limit`.
    /// Readings that hold only part of a long tag (a few characters hidden at
    /// one end) are fetched separately, so they never crowd out full matches.
    pub fn search_lines(&self, term: &Term, scope: &Scope, limit: usize) -> Result<Vec<Candidate>> {
        if term.fold.chars().count() < 3 {
            return Ok(Vec::new());
        }
        let (where_, sp) = scope.sql_from(2);
        let sql = format!(
            "SELECT l.id, l.file_id, l.page, l.text, l.conf, l.source, l.x, l.y, l.w, l.h
             FROM lines_fts JOIN lines l ON l.id=lines_fts.rowid JOIN files f ON f.id=l.file_id
             WHERE lines_fts MATCH ?1 AND f.read_state={}{where_} LIMIT ?2",
            state::READ
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut queries = vec![(format!("fold : {}", fts_phrase(&term.fold)), limit * 4)];
        let cuts = term.truncations();
        if !cuts.is_empty() {
            queries.push((cuts.iter().map(|c| format!("fold : {}", fts_phrase(c))).collect::<Vec<_>>().join(" OR "), limit));
        }
        for (q, cap) in queries {
            let mut args: Vec<rusqlite::types::Value> = vec![q.into(), (cap as i64).into()];
            args.extend(sp.iter().cloned().map(Into::into));
            let rows = st.query_map(params_from_iter(args), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, u32>(2)?, r.get::<_, String>(3)?, r.get::<_, f64>(4)? as f32, r.get::<_, i64>(5)?, bbox_of(r, 6)?))
            })?;
            for row in rows {
                let (id, file_id, page, text, conf, source, bbox) = row?;
                if !seen.insert(id) {
                    continue;
                }
                if let Some(d) = term.explain(&text, conf) {
                    out.push(Candidate { file_id, page: Some(page), text, conf, source: source_of(source), bbox: Some(bbox), class: d.class, why: d.why });
                }
            }
        }
        out.sort_by(|a, b| a.class.cmp(&b.class).then(b.conf.total_cmp(&a.conf)));
        out.truncate(limit);
        Ok(out)
    }

    /// Files whose name matches one term, inside `scope`, best first.
    ///
    /// Works like Everything: a case-insensitive substring of the name, of
    /// any length and any file type, with `*` and `?` wildcards; a term with
    /// `\` is compared with the full path (a pasted `\\server\share\...` or
    /// local `C:\...` path finds that file or folder). Names are exact text,
    /// so look-alike folding never applies; separators may differ
    /// ([`MatchClass::SeparatorsIgnored`]).
    pub fn search_names(&self, term: &Term, scope: &Scope, limit: usize) -> Result<Vec<Candidate>> {
        let rows = self.name_candidates(term, scope, limit * 4)?;
        let mut out: Vec<(bool, Candidate)> = rows.iter().filter_map(|(file_id, name, path)| name_candidate(term, *file_id, name, path)).collect();
        out.sort_by_key(|(whole, c)| (c.class, !*whole));
        out.truncate(limit);
        Ok(out.into_iter().map(|(_, c)| c).collect())
    }

    /// Files matching every term of `terms`, each term in the file name or
    /// anywhere in its folder path (`ISO 2632 HOLD`, `Rev-1 PSV-2809`).
    /// Candidates come from the most specific term, so no per-term limit can
    /// drop a file before the others are checked.
    pub fn search_names_all(&self, terms: &[Term], scope: &Scope, limit: usize) -> Result<Vec<Candidate>> {
        let Some(anchor) = terms.iter().max_by_key(|t| (t.is_path(), sep_form(t.literal()).chars().count())) else { return Ok(Vec::new()) };
        let cap = limit * 4;
        let mut seen = std::collections::HashSet::new();
        let mut rows = Vec::new();
        push_new(&mut seen, &mut rows, self.name_candidates(anchor, scope, cap)?);
        if !anchor.is_path() {
            push_new(&mut seen, &mut rows, self.files_under_dirs(&anchor.raw, scope, cap)?);
        }
        let mut out: Vec<Candidate> = rows.iter().filter_map(|(file_id, name, path)| all_terms_candidate(terms, *file_id, name, path)).collect();
        out.sort_by_key(|c| c.class);
        out.truncate(limit);
        Ok(out)
    }

    /// `(id, name, path)` of files that may match `term` by name or path,
    /// at most about `cap`, whole-name matches first.
    fn name_candidates(&self, term: &Term, scope: &Scope, cap: usize) -> Result<Vec<NameRow>> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        if term.is_path() {
            if looks_like_path(&term.raw) {
                let prefix = term.raw.split(['*', '?']).next().unwrap_or("");
                push_new(&mut seen, &mut out, self.paths_with_prefix(prefix, scope, cap)?);
            } else {
                let frag = term.raw.trim_end_matches('\\');
                let last = frag.rsplit('\\').next().unwrap_or("");
                if !term.raw.ends_with('\\')
                    && let Some(t) = Term::new(last)
                {
                    push_new(&mut seen, &mut out, self.whole_names(&t, scope, cap)?);
                    push_new(&mut seen, &mut out, self.names_containing(&t, scope, cap)?);
                }
                push_new(&mut seen, &mut out, self.files_under_dirs(frag, scope, cap)?);
            }
        } else {
            push_new(&mut seen, &mut out, self.whole_names(term, scope, cap)?);
            push_new(&mut seen, &mut out, self.names_containing(term, scope, cap)?);
        }
        Ok(out)
    }

    /// Files named exactly `term`, with or without an extension (indexed).
    /// The index compares ASCII case only; other names are found as whole
    /// names by [`Store::names_containing`], which ranks them first.
    fn whole_names(&self, term: &Term, scope: &Scope, cap: usize) -> Result<Vec<NameRow>> {
        if term.has_wildcards() || term.is_path() || !term.raw.is_ascii() {
            return Ok(Vec::new());
        }
        let (where_, sp) = scope.sql_from(3);
        let sql = format!(
            "SELECT f.id, f.name, f.path FROM files f WHERE f.name COLLATE NOCASE >= ?1 AND f.name COLLATE NOCASE < ?2 AND f.read_state<>{}{where_} LIMIT ?3",
            state::GONE
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let mut out = Vec::new();
        for (lo, hi) in [(term.raw.clone(), format!("{}\u{1}", term.raw)), (format!("{}.", term.raw), format!("{}.\u{10FFFF}", term.raw))] {
            let mut args: Vec<rusqlite::types::Value> = vec![lo.into(), hi.into(), (cap as i64).into()];
            args.extend(sp.iter().cloned().map(Into::into));
            for r in st.query_map(params_from_iter(args), name_row)? {
                let r = r?;
                if term.is_whole_name(&r.1, &r.2) {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    /// Files whose name contains the term, at most `cap`, best first: the
    /// whole name, then the term standing on its own, then with other
    /// separators, then inside a longer word. Candidates are read in index
    /// order and classified here (Unicode case-insensitively), so a common
    /// fragment (`Thumbs`, `_ACTIVIT`) cannot fill the cap with partial
    /// matches before the exact ones are seen. Three or more letters and
    /// digits use the trigram index; fewer scan the name index.
    fn names_containing(&self, term: &Term, scope: &Scope, cap: usize) -> Result<Vec<NameRow>> {
        let lit = fold_form(term.literal());
        let n = lit.chars().count();
        // A pre-filter in SQL, so rows that cannot match are skipped cheaply:
        // the text as typed (wildcards, or fewer than three letters and
        // digits), else its letters and digits in order. LIKE folds ASCII
        // case only, so other text uses GLOB with both cases of each letter.
        let spread = !term.has_wildcards() && n > 2;
        // `{c}` stands for the name column; the pattern is bound as ?2.
        let (filter, pat): (String, String) = if term.raw.is_ascii() {
            let body = if term.has_wildcards() {
                like_glob(&term.raw)
            } else if spread {
                term.sep.chars().map(|c| like_escape(&c.to_string())).collect::<Vec<_>>().join("%")
            } else {
                like_escape(&term.raw)
            };
            ("{c} LIKE ?2 ESCAPE '\\'".into(), format!("%{body}%"))
        } else if let Some(v) = (n <= 2 && !term.has_wildcards()).then(|| case_variants(&term.raw)).flatten() {
            // A short term is a scan of every name: plain substring tests of
            // each spelling are much faster than a GLOB with letter classes.
            let tests: Vec<String> = v.iter().map(|s| format!("instr({{c}}, '{}')>0", s.replace('\'', "''"))).collect();
            (format!("?2='' AND ({})", tests.join(" OR ")), String::new())
        } else {
            let body = if spread { term.sep.chars().map(|c| glob_char(c, false)).collect::<Vec<_>>().join("*") } else { term.raw.chars().map(|c| glob_char(c, term.has_wildcards())).collect() };
            ("{c} GLOB ?2".into(), format!("*{body}*"))
        };
        let mut best: [Vec<NameRow>; 4] = Default::default();
        let mut take = |row: NameRow| -> bool {
            if let Some(d) = term.explain_name(&row.1, &row.2) {
                let b = if term.is_whole_name(&row.1, &row.2) {
                    0
                } else {
                    match d.class {
                        MatchClass::Exact => 1,
                        MatchClass::SeparatorsIgnored => 2,
                        _ => 3,
                    }
                };
                if best[b].len() < cap {
                    best[b].push(row);
                }
            }
            // Enough of the best kinds: stop reading.
            best[0].len() + best[1].len() >= cap
        };
        let gone = format!("f.read_state<>{}", state::GONE);
        let mut scanned = 0usize;
        if n >= 3 {
            let (where_, sp) = scope.sql_from(2);
            let sql = format!("SELECT f.id, f.name, f.path FROM names JOIN files f ON f.id=names.rowid WHERE names MATCH ?1 AND ({}) AND {gone}{where_}", filter.replace("{c}", "f.name"));
            let mut st = self.conn.prepare_cached(&sql)?;
            let mut args: Vec<rusqlite::types::Value> = vec![format!("fold : {}", fts_phrase(&lit)).into(), pat.into()];
            args.extend(sp.into_iter().map(Into::into));
            for r in st.query_map(params_from_iter(args), name_row)? {
                scanned += 1;
                if take(r?) || scanned >= NAME_SCAN_BUDGET {
                    break;
                }
            }
        } else {
            // Below what the trigram index can look up: scan the name index
            // (names only, much smaller than the table). One or two
            // characters are in a large share of all names, so reading
            // stops sooner.
            let (where_, sp) = scope.sql_from(2);
            let sql = format!("SELECT f.id, f.name, f.path FROM files n INDEXED BY files_name JOIN files f ON f.id=n.id WHERE ?1 IS NULL AND ({}) AND {gone}{where_}", filter.replace("{c}", "n.name"));
            let mut st = self.conn.prepare_cached(&sql)?;
            let mut args: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Null, pat.into()];
            args.extend(sp.into_iter().map(Into::into));
            for r in st.query_map(params_from_iter(args), name_row)? {
                scanned += 1;
                if take(r?) || scanned >= SHORT_SCAN_BUDGET {
                    break;
                }
            }
        }
        let mut out: Vec<NameRow> = best.into_iter().flatten().collect();
        out.truncate(cap);
        Ok(out)
    }

    /// Files whose path starts with `prefix` (the unique path index).
    fn paths_with_prefix(&self, prefix: &str, scope: &Scope, cap: usize) -> Result<Vec<NameRow>> {
        let (where_, sp) = scope.sql_from(3);
        let sql = format!("SELECT f.id, f.name, f.path FROM files f WHERE f.path >= ?1 AND f.path < ?2 AND f.read_state<>{}{where_} LIMIT ?3", state::GONE);
        let mut st = self.conn.prepare_cached(&sql)?;
        let mut args: Vec<rusqlite::types::Value> = vec![prefix.to_string().into(), format!("{prefix}\u{10FFFF}").into(), (cap as i64).into()];
        args.extend(sp.into_iter().map(Into::into));
        Ok(st.query_map(params_from_iter(args), name_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Files in (or below) folders whose path contains `fragment` (`*` and
    /// `?` allowed).
    fn files_under_dirs(&self, fragment: &str, scope: &Scope, cap: usize) -> Result<Vec<NameRow>> {
        if sep_form(fragment).is_empty() {
            return Ok(Vec::new());
        }
        let dirs: Vec<String> = {
            // LIKE folds ASCII case only; GLOB with both cases otherwise.
            let (sql, pat) = if fragment.is_ascii() {
                ("SELECT path FROM dirs WHERE path LIKE ?1 ESCAPE '\\' LIMIT 2000", format!("%{}%", like_glob(fragment)))
            } else {
                ("SELECT path FROM dirs WHERE path GLOB ?1 LIMIT 2000", format!("*{}*", fragment.chars().map(|c| glob_char(c, true)).collect::<String>()))
            };
            let mut st = self.conn.prepare_cached(sql)?;
            st.query_map([pat], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let (where_, sp) = scope.sql_from(2);
        let sql = format!("SELECT f.id, f.name, f.path FROM files f WHERE f.dir=?1 AND f.read_state<>{}{where_} LIMIT ?2", state::GONE);
        let mut st = self.conn.prepare_cached(&sql)?;
        let mut out = Vec::new();
        for d in dirs {
            if out.len() >= cap {
                break;
            }
            let mut args: Vec<rusqlite::types::Value> = vec![d.into(), ((cap - out.len()) as i64).into()];
            args.extend(sp.iter().cloned().map(Into::into));
            for r in st.query_map(params_from_iter(args), name_row)? {
                out.push(r?);
            }
        }
        Ok(out)
    }

    /// Files inside `scope` (for searches made only of filters, e.g. `rev:B`).
    pub fn files_in_scope(&self, scope: &Scope, limit: usize) -> Result<Vec<i64>> {
        let (where_, sp) = scope.sql_from(1);
        let sql = format!("SELECT f.id FROM files f WHERE f.read_state<>{}{where_} ORDER BY f.mtime DESC LIMIT ?1", state::GONE);
        let mut st = self.conn.prepare_cached(&sql)?;
        let mut args: Vec<rusqlite::types::Value> = vec![(limit as i64).into()];
        args.extend(sp.into_iter().map(Into::into));
        Ok(st.query_map(params_from_iter(args), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Tag occurrences of one category whose tag contains the term (any
    /// length, separators ignored), inside `scope`. Used for `type:` searches,
    /// where short terms like `FV` would be too short for the text index.
    pub fn search_tags(&self, term: &Term, category: &str, scope: &Scope, limit: usize) -> Result<Vec<Candidate>> {
        let (where_, sp) = scope.sql_from(3);
        let sql = format!(
            "SELECT o.file_id, o.page, o.raw, o.conf, o.source, o.x, o.y, o.w, o.h
             FROM tag_occurrences o JOIN engineering_tags t ON t.id=o.tag_id JOIN files f ON f.id=o.file_id
             WHERE t.category=?1 AND t.norm LIKE ?2 ESCAPE '\\' AND f.read_state={}{where_} LIMIT {}",
            state::READ,
            limit * 4
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let mut args: Vec<rusqlite::types::Value> = vec![category.to_lowercase().into(), format!("%{}%", like_escape(&term.sep.to_uppercase())).into()];
        args.extend(sp.into_iter().map(Into::into));
        let rows = st.query_map(params_from_iter(args), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?, r.get::<_, f64>(3)? as f32, r.get::<_, i64>(4)?, bbox_of(r, 5)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (file_id, page, text, conf, source, bbox) = row?;
            if let Some(d) = term.explain(&text, conf) {
                out.push(Candidate { file_id, page: Some(page), text, conf, source: source_of(source), bbox: Some(bbox), class: d.class, why: format!("{category} tag: {}", d.why.to_lowercase()) });
            }
        }
        out.sort_by(|a, b| a.class.cmp(&b.class).then(b.conf.total_cmp(&a.conf)));
        out.truncate(limit);
        Ok(out)
    }

    /// Does the file have a tag of `category` whose form contains `sep`?
    pub fn file_has_tag(&self, file_id: i64, category: &str, sep: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tag_occurrences o JOIN engineering_tags t ON t.id=o.tag_id WHERE o.file_id=?1 AND t.category=?2 AND t.norm LIKE ?3 ESCAPE '\\')",
            params![file_id, category, format!("%{}%", like_escape(&sep.to_uppercase()))],
            |r| r.get(0),
        )?)
    }

    /// Lines on one page of a file that match any term (for preview boxes).
    pub fn page_boxes(&self, file_id: i64, page: u32, terms: &[Term]) -> Result<Vec<NormBox>> {
        let mut st = self.conn.prepare_cached("SELECT text, conf, x, y, w, h FROM lines WHERE file_id=?1 AND page=?2")?;
        let rows = st.query_map(params![file_id, page], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)? as f32, bbox_of(r, 2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (text, conf, b) = row?;
            if terms.iter().any(|t| t.classify(&text, conf).is_some()) {
                out.push(b);
            }
        }
        Ok(out)
    }

    /// Boxes of every occurrence of one tag on one page.
    pub fn tag_boxes(&self, file_id: i64, page: u32, norm: &str) -> Result<Vec<NormBox>> {
        let mut st = self.conn.prepare_cached("SELECT o.x, o.y, o.w, o.h FROM tag_occurrences o JOIN engineering_tags t ON t.id=o.tag_id WHERE o.file_id=?1 AND o.page=?2 AND t.norm=?3")?;
        Ok(st.query_map(params![file_id, page, norm], |r| bbox_of(r, 0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn metadata(&self, file_id: i64) -> Result<DrawingMeta> {
        Ok(self
            .conn
            .prepare_cached("SELECT drawing_no, title, revision, project, discipline, sheet, status, evidence FROM drawing_metadata WHERE file_id=?1")?
            .query_row(
                [file_id],
                |r| {
                    Ok(DrawingMeta {
                        drawing_no: r.get(0)?,
                        title: r.get(1)?,
                        revision: r.get(2)?,
                        project: r.get(3)?,
                        discipline: r.get(4)?,
                        sheet: r.get(5)?,
                        status: r.get(6)?,
                        evidence: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    // ---- status -----------------------------------------------------------

    pub fn counts(&self) -> Result<(u64, u64, u64, u64)> {
        let mut st = self.conn.prepare_cached("SELECT read_state, COUNT(*) FROM files GROUP BY read_state")?;
        let (mut all, mut read, mut waiting, mut failed) = (0u64, 0u64, 0u64, 0u64);
        for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64)))? {
            let (s, n) = row?;
            if s != state::GONE {
                all += n;
            }
            match s {
                state::READ => read += n,
                state::WAITING => waiting += n,
                state::FAILED => failed += n,
                _ => {}
            }
        }
        Ok((all, read, waiting, failed))
    }

    pub fn failures(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT path, COALESCE(read_error,'') FROM files WHERE read_state=?1 ORDER BY read_at DESC LIMIT ?2")?;
        let rows = st.query_map(params![state::FAILED, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Mark PDFs under the given folders as waiting and the others as unread;
    /// PDFs under the office server's folders are left to the server.
    pub fn apply_scopes(&self, scopes: &[String], office: &[String]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        // Only the files whose state changes are written: resetting every
        // waiting PDF and setting it back rewrote tens of thousands of rows
        // at every start, holding the index for minutes on a hard disk.
        let inside = |dir: &str, folders: &[String]| {
            let d = dir.to_lowercase();
            folders.iter().any(|f| {
                let f = f.trim_end_matches('\\').to_lowercase();
                d == f || d.starts_with(&format!("{f}\\"))
            })
        };
        let leaving: Vec<i64> = {
            let mut st = tx.prepare("SELECT id, dir, read_state FROM files WHERE read_state IN (?1, ?2)")?;
            let rows = st.query_map(params![state::WAITING, state::OFFICE], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (id, dir, st) = row?;
                let keep = if st == state::OFFICE { inside(&dir, office) } else { inside(&dir, scopes) && !inside(&dir, office) };
                if !keep {
                    out.push(id);
                }
            }
            out
        };
        for id in leaving {
            tx.execute("UPDATE files SET read_state=?1 WHERE id=?2", params![state::UNREAD, id])?;
        }
        for s in scopes {
            let s = s.trim_end_matches('\\');
            tx.execute(
                "UPDATE files SET read_state=?1 WHERE read_state=?2 AND (dir=?3 OR dir LIKE ?4 ESCAPE '\\')",
                params![state::WAITING, state::UNREAD, s, format!("{}\\\\%", like_escape(s))],
            )?;
        }
        for s in office {
            let s = s.trim_end_matches('\\');
            tx.execute(
                "UPDATE files SET read_state=?1 WHERE read_state IN (?2, ?3) AND (dir=?4 OR dir LIKE ?5 ESCAPE '\\')",
                params![state::OFFICE, state::WAITING, state::UNREAD, s, format!("{}\\\\%", like_escape(s))],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO meta(key, value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        Ok(())
    }

    /// Operational history for the monitor page; the newest 500 are kept.
    pub fn event(&self, now: i64, level: &str, message: &str) {
        let _ = self.conn.execute("INSERT INTO index_events(at, level, message) VALUES(?1,?2,?3)", params![now, level, message]);
        let _ = self.conn.execute("DELETE FROM index_events WHERE id <= (SELECT MAX(id) - 500 FROM index_events)", []);
    }

    pub fn events(&self, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT at, level, message FROM index_events ORDER BY id DESC LIMIT ?1")?;
        Ok(st.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_root_scan(&self, root: &str, state: &str, ok_at: Option<i64>, note: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO root_scans(root, state, last_ok, note) VALUES(?1,?2,COALESCE(?3,0),?4)
             ON CONFLICT(root) DO UPDATE SET state=excluded.state, note=excluded.note, last_ok=COALESCE(?3, root_scans.last_ok)",
            params![root, state, ok_at, note],
        )?;
        Ok(())
    }

    pub fn root_scans(&self) -> Result<Vec<(String, String, i64, String)>> {
        let mut st = self.conn.prepare_cached("SELECT root, state, last_ok, note FROM root_scans ORDER BY root")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn checkpoint(&self, volume: &str) -> Result<Option<(String, i64)>> {
        Ok(self.conn.query_row("SELECT journal_id, next_usn FROM volume_checkpoints WHERE volume=?1", [volume], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    pub fn set_checkpoint(&self, volume: &str, journal_id: &str, next_usn: i64, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO volume_checkpoints(volume, journal_id, next_usn, updated) VALUES(?1,?2,?3,?4)
             ON CONFLICT(volume) DO UPDATE SET journal_id=excluded.journal_id, next_usn=excluded.next_usn, updated=excluded.updated",
            params![volume, journal_id, next_usn, now],
        )?;
        Ok(())
    }

    pub fn add_history(&self, query: &str, now: i64) -> Result<()> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(());
        }
        self.conn.execute("INSERT INTO search_history(query, at) VALUES(?1,?2) ON CONFLICT(query) DO UPDATE SET at=excluded.at", params![q, now])?;
        self.conn.execute("DELETE FROM search_history WHERE pinned=0 AND query NOT IN (SELECT query FROM search_history WHERE pinned=0 ORDER BY at DESC LIMIT 100)", [])?;
        Ok(())
    }

    pub fn history(&self) -> Result<Vec<(String, i64, bool)>> {
        let mut st = self.conn.prepare_cached("SELECT query, at, pinned FROM search_history ORDER BY pinned DESC, at DESC LIMIT 60")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn pin(&self, query: &str, pinned: bool, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO search_history(query, at, pinned) VALUES(?1,?2,?3) ON CONFLICT(query) DO UPDATE SET pinned=excluded.pinned",
            params![query.trim(), now, pinned as i64],
        )?;
        Ok(())
    }

    /// Mirror the settings' projects into tables, for SQL joins.
    pub fn set_projects(&self, projects: &[sleuth_core::settings::Project]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM projects", [])?;
        for p in projects {
            if p.name.trim().is_empty() {
                continue;
            }
            tx.execute("INSERT OR IGNORE INTO projects(name) VALUES(?1)", [p.name.trim()])?;
            let id: i64 = tx.query_row("SELECT id FROM projects WHERE name=?1", [p.name.trim()], |r| r.get(0))?;
            for r in &p.roots {
                tx.execute("INSERT INTO project_roots(project_id, root) VALUES(?1,?2)", params![id, r.trim_end_matches('\\')])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// Names of file types without a title block, as SQL on `f.name`.
const NO_TITLE_BLOCK_SQL: &str = "lower(f.name) LIKE '%.msg' OR lower(f.name) LIKE '%.eml' OR lower(f.name) LIKE '%.txt' OR lower(f.name) LIKE '%.csv' OR lower(f.name) LIKE '%.tsv' OR lower(f.name) LIKE '%.log'";

/// Replace a file's tags and title-block details from its stored lines.
fn derive_into(tx: &rusqlite::Transaction, file_id: i64, name: &str, d: &Deriver) -> Result<()> {
    tx.execute("DELETE FROM tag_occurrences WHERE file_id=?1", [file_id])?;
    tx.execute("DELETE FROM drawing_metadata WHERE file_id=?1", [file_id])?;
    let pages: Vec<(u32, Line)> = {
        let mut st = tx.prepare_cached("SELECT page, text, conf, source, alt, x, y, w, h FROM lines WHERE file_id=?1 ORDER BY page, id")?;
        st.query_map([file_id], |r| {
            Ok((r.get::<_, u32>(0)?, Line { text: r.get(1)?, conf: r.get::<_, f64>(2)? as f32, source: source_of(r.get(3)?), alt: r.get::<_, i64>(4)? != 0, bbox: bbox_of(r, 5)? }))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    // An OCR line a person corrected is represented by the correction.
    let verified: Vec<(u32, NormBox)> = pages.iter().filter(|(_, l)| l.source == TextSource::Verified).map(|(p, l)| (*p, l.bbox)).collect();
    let corrected = |p: u32, l: &Line| l.source == TextSource::Ocr && verified.iter().any(|(vp, b)| *vp == p && b.overlap(&l.bbox) >= 0.8);
    {
        let mut tag = tx.prepare_cached("INSERT INTO engineering_tags(norm, category, fold) VALUES(?1,?2,?3) ON CONFLICT(norm) DO NOTHING")?;
        let mut tag_id = tx.prepare_cached("SELECT id FROM engineering_tags WHERE norm=?1")?;
        let mut occ = tx.prepare_cached("INSERT INTO tag_occurrences(tag_id, file_id, page, raw, conf, source, x, y, w, h, parser) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?;
        for (page, l) in &pages {
            if l.alt || corrected(*page, l) {
                continue;
            }
            for t in d.tags(&l.text) {
                tag.execute(params![t.norm, t.category, fold_form(&t.norm)])?;
                let id: i64 = tag_id.query_row([&t.norm], |r| r.get(0))?;
                occ.execute(params![id, file_id, page, t.raw, l.conf as f64, source_code(l.source), l.bbox.x as f64, l.bbox.y as f64, l.bbox.w as f64, l.bbox.h as f64, d.hash])?;
            }
        }
    }
    let lines: Vec<(u32, &Line)> = pages.iter().filter(|(p, l)| !l.alt && !corrected(*p, l)).map(|(p, l)| (*p, l)).collect();
    let m = if sleuth_core::kinds::FileKind::of(name).has_title_block() { d.metadata(name, &lines) } else { DrawingMeta::default() };
    if m.drawing_no.is_some() || m.revision.is_some() || m.title.is_some() {
        tx.execute(
            "INSERT INTO drawing_metadata(file_id, drawing_no, drawing_norm, title, revision, revision_norm, project, discipline, sheet, status, evidence)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                file_id,
                m.drawing_no,
                m.drawing_no.as_deref().map(norm_form),
                m.title,
                m.revision,
                m.revision.as_deref().map(norm_form),
                m.project,
                m.discipline,
                m.sheet,
                m.status,
                serde_json::to_string(&m.evidence)?
            ],
        )?;
    }
    tx.execute("UPDATE files SET derived=?2 WHERE id=?1", params![file_id, d.hash])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::PageText;
    use sleuth_core::analyzer::parse_query;

    fn line(text: &str, conf: f32, source: TextSource) -> Line {
        Line { text: text.into(), conf, source, bbox: NormBox { x: 0.1, y: 0.2, w: 0.1, h: 0.01 }, alt: false }
    }

    fn ft(page: u32, lines: Vec<Line>) -> FileText {
        FileText { reader_version: 2, model: "m".into(), pages: vec![PageText { page, lines }], ..Default::default() }
    }

    fn d() -> Deriver {
        Deriver::new(&sleuth_core::settings::Settings::default())
    }

    #[test]
    fn content_search_classes() {
        let mut s = Store::open_in_memory().unwrap();
        let (a, _) = s.upsert_file(r"\\srv\p\iso1.pdf", r"\\srv\p", "iso1.pdf", 10, 1, 1, state::WAITING).unwrap();
        let (b, _) = s.upsert_file(r"\\srv\p\scan.pdf", r"\\srv\p", "scan.pdf", 10, 1, 1, state::WAITING).unwrap();
        s.save_read(a, "h1", &ft(1, vec![line("LINE 10\"-P-1001-A1B CS", 1.0, TextSource::TextLayer)]), 5, &d()).unwrap();
        s.save_read(b, "h2", &ft(2, vec![line("10\"-P-l001-A1B", 0.95, TextSource::Ocr)]), 5, &d()).unwrap();
        let t = &parse_query("10\"-P-1001-A1B")[0];
        let hits = s.search_lines(t, &Scope::default(), 50).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].class, MatchClass::Exact);
        assert_eq!(hits[1].class, MatchClass::Uncertain);
        assert_eq!(hits[1].page, Some(2));
    }

    #[test]
    fn emails_and_text_files_have_no_title_block() {
        let mut s = Store::open_in_memory().unwrap();
        let said = vec![line("The outlet line is added in line list rev 4", 1.0, TextSource::TextLayer), line("DRAWING No.: ISO-1001-01", 1.0, TextSource::TextLayer)];
        let mut ids = Vec::new();
        for name in ["note.pdf", "RE PSV-1203.eml", "RE PSV-1203.msg", "index.csv", "notes.txt"] {
            let (id, _) = s.upsert_file(&format!(r"\\srv\p\{name}"), r"\\srv\p", name, 1, 1, 1, state::WAITING).unwrap();
            s.save_read(id, name, &ft(1, said.clone()), 1, &d()).unwrap();
            ids.push(id);
        }
        assert_eq!(s.metadata(ids[0]).unwrap().drawing_no.as_deref(), Some("ISO-1001-01"), "a PDF keeps its title block");
        for id in &ids[1..] {
            let m = s.metadata(*id).unwrap();
            assert!(m.drawing_no.is_none() && m.revision.is_none(), "{id}: {m:?}");
        }
    }

    #[test]
    fn parts_hide_title_blocks_of_emails_written_by_1_1_0() {
        let dir = std::env::temp_dir().join(format!("sl-tb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("part.db");
        let (pdf, eml) = {
            let s = Store::create_bulk(&p).unwrap();
            let (pdf, _) = s.upsert_file(r"\\srv\p\iso.pdf", r"\\srv\p", "iso.pdf", 1, 1, 1, state::WAITING).unwrap();
            let (eml, _) = s.upsert_file(r"\\srv\p\RE iso.EML", r"\\srv\p", "RE iso.EML", 1, 1, 1, state::WAITING).unwrap();
            // what 1.1.0 wrote for both
            for id in [pdf, eml] {
                s.conn.execute("INSERT INTO drawing_metadata(file_id, drawing_no, drawing_norm, revision, revision_norm, evidence) VALUES(?1,'ISO-1001-01','ISO10010','4','4','[]')", [id]).unwrap();
            }
            (pdf, eml)
        };
        let part = Store::open_part(&p).unwrap();
        assert_eq!(part.metadata(pdf).unwrap().revision.as_deref(), Some("4"));
        let m = part.metadata(eml).unwrap();
        assert!(m.drawing_no.is_none() && m.revision.is_none(), "{m:?}");
        assert_eq!(part.metadata_many(&[pdf, eml]).unwrap().len(), 1);
        drop(part);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scope_is_applied_before_the_limit() {
        // Many matches outside the chosen folder must not crowd out the one inside.
        let mut s = Store::open_in_memory().unwrap();
        for i in 0..60 {
            let (id, _) = s.upsert_file(&format!(r"\\srv\other\f{i}.pdf"), r"\\srv\other", &format!("f{i}.pdf"), 1, 1, 1, state::WAITING).unwrap();
            s.save_read(id, &format!("o{i}"), &ft(1, vec![line("PSV-1203", 1.0, TextSource::TextLayer)]), 1, &d()).unwrap();
        }
        let (mine, _) = s.upsert_file(r"\\srv\proj\a\iso.pdf", r"\\srv\proj\a", "iso.pdf", 1, 1, 1, state::WAITING).unwrap();
        s.save_read(mine, "m", &ft(1, vec![line("PSV-1203", 1.0, TextSource::TextLayer)]), 1, &d()).unwrap();
        let t = &parse_query("PSV-1203")[0];
        let hits = s.search_lines(t, &Scope::roots(&[r"\\srv\proj".into()]), 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_id, mine);
        // "\\srv\pro" is not a parent of "\\srv\proj"
        assert!(s.search_lines(t, &Scope::roots(&[r"\\srv\pro".into()]), 5).unwrap().is_empty());
    }

    #[test]
    fn names_and_reconcile() {
        let s = Store::open_in_memory().unwrap();
        s.touch_dir(r"C:\d", None, 1).unwrap();
        s.upsert_file(r"C:\d\PSV-1203 datasheet.pdf", r"C:\d", "PSV-1203 datasheet.pdf", 1, 1, 1, state::UNREAD).unwrap();
        s.upsert_file(r"C:\d\old.pdf", r"C:\d", "old.pdf", 1, 1, 1, state::UNREAD).unwrap();
        let t = &parse_query("psv1203")[0];
        assert_eq!(s.search_names(t, &Scope::default(), 10).unwrap()[0].class, MatchClass::SeparatorsIgnored);
        s.upsert_file(r"C:\d\PSV-1203 datasheet.pdf", r"C:\d", "PSV-1203 datasheet.pdf", 1, 1, 2, state::UNREAD).unwrap();
        s.reconcile_dir(r"C:\d", 2, 10).unwrap();
        assert!(s.file_by_path(r"C:\d\old.pdf").unwrap().is_none());
        assert!(s.search_names(&parse_query("old")[0], &Scope::default(), 10).unwrap().is_empty());
    }

    /// The user's index: names of every type, two copies of one iso on the share.
    fn office_names() -> Store {
        let s = Store::open_in_memory().unwrap();
        let share = r"\\10.0.0.54\docs-latest\00_DOCUMENTS\Isometrics\Rev-1";
        for (dir, name, st) in [
            (r"D:", "FSK_Piping-Layout and GAD-Final Submission.zip", state::NOT_PDF),
            (r"D:", "FSK As-Built request_18-Sep-2026.xlsx", state::NOT_PDF),
            (r"D:\lots", "FSK_Batch-1.zip", state::NOT_PDF),
            (r"D:\lots", "FSK_Batch-12.zip", state::NOT_PDF),
            (r"D:\lots", "Other FS notes.dwg", state::NOT_PDF),
            (r"D:\lots", "FS", state::NOT_PDF),
            (r"\\10.0.0.20\Project\P20002-14-01-63-1636\MAIN DOC", "P20002-14-01-63-1636-REV.2.xlsx", state::NOT_PDF),
            (r"\\10.0.0.20\Project\P20002-14-01-63-1636\RELATED FILES", "CRS-14-01-63-1636_1.pdf", state::UNREAD),
            (r"\\10.0.0.20\Project\x", "ISO-1636.pdf", state::UNREAD),
            (format!(r"{share}\26-02-2024\14-01-15-2632").as_str(), "14-01-15-2632_PSV-2809-01_HOLD.pdf", state::UNREAD),
            (format!(r"{share}\26-02-2024\ISOMETRIC ISSUANCE_26-FEB-2024\14-01-15-2632").as_str(), "14-01-15-2632_PSV-2809-01_HOLD.pdf", state::UNREAD),
            (r"C:\x", "notes.txt", state::NOT_PDF),
        ] {
            s.touch_dir(dir, None, 1).unwrap();
            s.upsert_file(&format!(r"{dir}\{name}"), dir, name, 1, 1, 1, st).unwrap();
        }
        s
    }

    fn names(s: &Store, q: &str) -> Vec<(String, MatchClass)> {
        let t = &parse_query(q)[0];
        s.search_names(t, &Scope::default(), 100).unwrap().into_iter().map(|c| (c.text, c.class)).collect()
    }

    #[test]
    fn names_like_everything_any_length_and_type() {
        let s = office_names();
        // exact name with punctuation, a zip: the whole name first, then longer names
        let r = names(&s, "FSK_Batch-1");
        assert_eq!(r[0], ("FSK_Batch-1.zip".into(), MatchClass::Exact));
        assert!(r.contains(&("FSK_Batch-12.zip".into(), MatchClass::Partial)));
        // separators differ
        assert_eq!(names(&s, "FSKBatch1")[0], ("FSK_Batch-1.zip".into(), MatchClass::SeparatorsIgnored));
        // three letters: zip, xlsx; never notes.txt
        let r = names(&s, "FSK");
        assert_eq!(r.iter().filter(|(_, c)| *c == MatchClass::Exact).count(), 4, "{r:?}");
        assert_eq!(r.len(), 4, "{r:?}");
        // two letters (below the trigram index) and one
        let r = names(&s, "FS");
        assert_eq!(r[0], ("FS".into(), MatchClass::Exact), "a file named exactly FS first: {r:?}");
        assert!(r.contains(&("Other FS notes.dwg".into(), MatchClass::Exact)));
        assert!(r.contains(&("FSK_Batch-1.zip".into(), MatchClass::Partial)));
        assert_eq!(r.len(), 6, "{r:?}");
        assert!(names(&s, "x").iter().any(|(n, _)| n == "P20002-14-01-63-1636-REV.2.xlsx"));
        // digits, any type
        let r = names(&s, "1636");
        assert_eq!(r.len(), 3, "{r:?}");
        assert!(r.iter().all(|(_, c)| *c == MatchClass::Exact));
        // wildcards: whole name first, part of a name after
        let r = names(&s, "*1636");
        assert_eq!(r[0], ("ISO-1636.pdf".into(), MatchClass::Exact), "{r:?}");
        assert_eq!(r.len(), 3, "{r:?}");
        let r = names(&s, "fsk_batch-?.zip");
        assert_eq!(r, vec![("FSK_Batch-1.zip".into(), MatchClass::Exact)]);
        assert_eq!(names(&s, "*.dwg"), vec![("Other FS notes.dwg".into(), MatchClass::Exact)]);
        assert_eq!(names(&s, "PSV").len(), 2);
    }

    #[test]
    fn names_by_path() {
        let s = office_names();
        let full = r"\\10.0.0.54\docs-latest\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf";
        let t = Term::new(full).unwrap();
        let r = s.search_names(&t, &Scope::default(), 10).unwrap();
        assert_eq!(r.len(), 1, "only that copy, not the one in ISSUANCE: {r:?}");
        assert_eq!(r[0].class, MatchClass::Exact);
        // relative pieces of a path, with and without the name
        let r = names(&s, r"14-01-15-2632\14-01-15-2632_PSV");
        assert_eq!(r.len(), 2, "{r:?}");
        let r = names(&s, r"26-02-2024\14-01-15-2632\");
        assert_eq!(r.len(), 1, "{r:?}");
        let r = names(&s, r#""MAIN DOC\""#);
        assert_eq!(r, vec![("P20002-14-01-63-1636-REV.2.xlsx".into(), MatchClass::Exact)]);
        assert_eq!(names(&s, r"\\10.0.0.20\project\*.pdf").len(), 2);
    }

    #[test]
    fn several_words_in_name_or_path() {
        let s = office_names();
        let q = parse_query("ISSUANCE PSV-2809 hold");
        let r = s.search_names_all(&q, &Scope::default(), 10).unwrap();
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].why.contains("folder path"));
        assert_eq!(s.search_names_all(&parse_query("FSK zip"), &Scope::default(), 10).unwrap().len(), 3);
        assert!(s.search_names_all(&parse_query("FSK dwg"), &Scope::default(), 10).unwrap().is_empty());
    }

    fn store_of(names: &[&str]) -> Store {
        let s = Store::open_in_memory().unwrap();
        for (i, n) in names.iter().enumerate() {
            let dir = format!(r"C:\d{i}");
            s.upsert_file(&format!(r"{dir}\{n}"), &dir, n, 1, 1, 1, state::NOT_PDF).unwrap();
        }
        s
    }

    #[test]
    fn punctuation_and_short_literal_names() {
        let s = store_of(&["a_b.pdf", "a&b.dwg", "ab.pdf", "PSV-1.pdf", "PSV-12.pdf", "PSV1.pdf", "1.pdf"]);
        let r = names(&s, "_");
        assert_eq!(r, vec![("a_b.pdf".into(), MatchClass::Exact)]);
        assert_eq!(names(&s, "&"), vec![("a&b.dwg".into(), MatchClass::Exact)]);
        let r = names(&s, "-1");
        assert_eq!(r, vec![("PSV-1.pdf".into(), MatchClass::Exact), ("PSV-12.pdf".into(), MatchClass::Partial)], "the dash is required");
    }

    #[test]
    fn names_match_unicode_case() {
        let s = store_of(&["Обычный.pmvx", "Įprastasis.pmvx", "ОБЫЧНЫЙ отчёт.xlsx", "Ob.pdf"]);
        let r = names(&s, "обычный");
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0], ("Обычный.pmvx".into(), MatchClass::Exact), "the whole name first");
        assert_eq!(names(&s, "įprastasis"), vec![("Įprastasis.pmvx".into(), MatchClass::Exact)]);
        assert_eq!(names(&s, "ĮPRAST").len(), 1);
        // fewer than three letters, too
        assert_eq!(names(&s, "об").len(), 2);
        assert_eq!(names(&s, "į").len(), 1);
        assert_eq!(names(&s, "ob").len(), 1);
        // and folder paths
        let s2 = Store::open_in_memory().unwrap();
        s2.touch_dir(r"C:\Отчёты", None, 1).unwrap();
        s2.upsert_file(r"C:\Отчёты\a.pdf", r"C:\Отчёты", "a.pdf", 1, 1, 1, state::NOT_PDF).unwrap();
        assert_eq!(s2.search_names_all(&parse_query("ОТЧЁТЫ a"), &Scope::default(), 10).unwrap().len(), 1);
    }

    #[test]
    fn a_name_search_leaves_no_read_transaction_open() {
        // A long-lived search connection must not stop WAL checkpoints.
        let dir = std::env::temp_dir().join(format!("sl-wal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("index.db");
        let w = Store::open(&p).unwrap();
        for i in 0..50 {
            w.upsert_file(&format!(r"C:\d\Thumbs{i}.db"), r"C:\d", &format!("Thumbs{i}.db"), 1, 1, 1, state::NOT_PDF).unwrap();
        }
        let r = Store::open(&p).unwrap();
        for q in ["Thumbs", "th", "_", "Thumbs1 db"] {
            let terms = parse_query(q);
            r.search_names(&terms[0], &Scope::default(), 2).unwrap();
            r.search_names_all(&terms, &Scope::default(), 2).unwrap();
        }
        assert!(r.conn.is_autocommit());
        w.upsert_file(r"C:\d\new.db", r"C:\d", "new.db", 1, 1, 1, state::NOT_PDF).unwrap();
        let busy: i64 = w.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0)).unwrap();
        assert_eq!(busy, 0, "a reader still holds a snapshot");
        drop((r, w));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn common_fragment_keeps_exact_names_before_the_cap() {
        // Many partial matches first in the index, the exact ones last.
        let mut list: Vec<String> = (0..300).map(|i| format!("X_ACTIVITY{i}.log")).collect();
        list.extend(["Thumbs.db".to_string(), "PLAN_ACTIVIT_A.pdf".to_string()]);
        list.extend((0..300).map(|i| format!("Thumbsnail{i}.png")));
        list.push("old Thumbs.db".into());
        let refs: Vec<&str> = list.iter().map(String::as_str).collect();
        let s = store_of(&refs);
        let t = &parse_query("_ACTIVIT")[0];
        let r = s.search_names(t, &Scope::default(), 5).unwrap();
        assert_eq!(r[0].text, "PLAN_ACTIVIT_A.pdf");
        assert_eq!(r[0].class, MatchClass::Exact);
        let t = &parse_query("Thumbs")[0];
        let r = s.search_names(t, &Scope::default(), 5).unwrap();
        assert_eq!(r[0].text, "Thumbs.db");
        assert_eq!(r[1].text, "old Thumbs.db");
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn moved_file_keeps_its_reading() {
        let mut s = Store::open_in_memory().unwrap();
        let (a, _) = s.upsert_file(r"C:\a\iso.pdf", r"C:\a", "iso.pdf", 5, 1, 1, state::WAITING).unwrap();
        s.save_read(a, "H", &ft(1, vec![line("PSV-1203", 1.0, TextSource::TextLayer)]), 1, &d()).unwrap();
        s.reconcile_dir(r"C:\a", 2, 100).unwrap();
        // gone: not searchable, but its lines are kept for a copy with the same content
        assert_eq!(s.file_by_id(a).unwrap().unwrap().read_state, state::GONE);
        assert!(s.search_lines(&parse_query("PSV-1203")[0], &Scope::default(), 5).unwrap().is_empty());
        let (b, _) = s.upsert_file(r"C:\b\iso.pdf", r"C:\b", "iso.pdf", 5, 1, 2, state::WAITING).unwrap();
        assert_eq!(s.read_copy_by_hash("H", b).unwrap(), Some(a));
        // back in its old place, unchanged: read again at once
        s.upsert_file(r"C:\a\iso.pdf", r"C:\a", "iso.pdf", 5, 1, 3, state::WAITING).unwrap();
        assert_eq!(s.file_by_id(a).unwrap().unwrap().read_state, state::READ);
        // purged after the grace period
        s.reconcile_dir(r"C:\a", 4, 100).unwrap();
        assert_eq!(s.purge_gone(100 + GONE_KEEP_SECS + 1).unwrap(), 1);
        assert!(s.file_by_id(a).unwrap().is_none());
    }

    #[test]
    fn changed_file_is_requeued_unchanged_is_not() {
        let mut s = Store::open_in_memory().unwrap();
        let (a, _) = s.upsert_file(r"C:\a.pdf", r"C:", "a.pdf", 10, 1, 1, state::WAITING).unwrap();
        s.save_read(a, "h", &FileText::default(), 1, &d()).unwrap();
        let (_, changed) = s.upsert_file(r"C:\a.pdf", r"C:", "a.pdf", 10, 1, 2, state::WAITING).unwrap();
        assert!(!changed, "an unchanged file produces no reading job");
        assert_eq!(s.file_by_id(a).unwrap().unwrap().read_state, state::READ);
        let (_, changed) = s.upsert_file(r"C:\a.pdf", r"C:", "a.pdf", 11, 2, 3, state::WAITING).unwrap();
        assert!(changed);
        assert_eq!(s.file_by_id(a).unwrap().unwrap().read_state, state::WAITING);
    }

    #[test]
    fn leases_hand_out_each_job_once_and_expire() {
        let s = Store::open_in_memory().unwrap();
        let (a, _) = s.upsert_file(r"C:\a.pdf", r"C:", "a.pdf", 1, 1, 1, state::WAITING).unwrap();
        assert_eq!(s.claim_next(100, "t1").unwrap().unwrap().id, a);
        assert!(s.claim_next(101, "t2").unwrap().is_none(), "leased");
        assert_eq!(s.claim_next(100 + LEASE_SECS, "t2").unwrap().unwrap().id, a, "expired lease is free again");
        s.release(a).unwrap();
        assert!(s.claim_next(200, "t3").unwrap().is_some());
    }

    #[test]
    fn upgrade_from_level_1_keeps_data() {
        let dir = std::env::temp_dir().join(format!("sl-upg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("index.db");
        {
            // the Sleuth 0.1 schema with one read file
            let c = Connection::open(&p).unwrap();
            c.execute_batch(
                "CREATE TABLE dirs(path TEXT PRIMARY KEY COLLATE NOCASE, parent TEXT COLLATE NOCASE, seen INTEGER NOT NULL);
                 CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE COLLATE NOCASE, dir TEXT NOT NULL COLLATE NOCASE, name TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL, seen INTEGER NOT NULL, read_state INTEGER NOT NULL DEFAULT 0, read_error TEXT, hash TEXT, pages INTEGER, model TEXT, read_at INTEGER, priority INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0, retry_at INTEGER);
                 CREATE VIRTUAL TABLE names USING fts5(fold, tokenize='trigram', content='', contentless_delete=1);
                 CREATE TABLE lines(id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE, page INTEGER NOT NULL, text TEXT NOT NULL, conf REAL NOT NULL, source INTEGER NOT NULL, alt INTEGER NOT NULL DEFAULT 0, x REAL, y REAL, w REAL, h REAL);
                 CREATE VIRTUAL TABLE lines_fts USING fts5(fold, tokenize='trigram', content='', contentless_delete=1);
                 CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
                 INSERT INTO files(id, path, dir, name, size, mtime, seen, read_state, hash) VALUES(1, 'C:\\x\\a.pdf', 'C:\\x', 'a.pdf', 1, 1, 1, 2, 'H');
                 INSERT INTO lines(id, file_id, page, text, conf, source, x, y, w, h) VALUES(1, 1, 1, 'PSV-1203', 1.0, 1, 0, 0, 0.1, 0.01);
                 INSERT INTO lines_fts(rowid, fold) VALUES(1, 'p5v1203');
                 PRAGMA user_version=1;",
            )
            .unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert_eq!(s.get_meta("schema").unwrap(), Some(SCHEMA_LEVEL.to_string()));
        assert!(p.with_extension("v1.bak.db").exists(), "level-1 copy kept");
        let hits = s.search_lines(&parse_query("PSV-1203")[0], &Scope::default(), 5).unwrap();
        assert_eq!(hits.len(), 1, "old readings still searchable");
        let v: i64 = s.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 1, "Sleuth 0.1 can still open it after a rollback");
        // stale derived data is found for re-deriving
        assert_eq!(s.stale_derived("x", 10).unwrap(), vec![1]);
        drop(s);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrections_survive_a_reread_of_the_same_content() {
        let mut s = Store::open_in_memory().unwrap();
        let (a, _) = s.upsert_file(r"C:\a.pdf", r"C:", "a.pdf", 1, 1, 1, state::WAITING).unwrap();
        let text = ft(1, vec![line("PSV-12O3", 0.6, TextSource::Ocr)]);
        s.save_read(a, "H", &text, 1, &d()).unwrap();
        s.conn
            .execute(
                "INSERT INTO corrections(file_id, file_hash, page, x, y, w, h, original, original_conf, corrected, user, at) VALUES(?1,'H',1,0.1,0.2,0.1,0.01,'PSV-12O3',0.6,'PSV-1203','me',1)",
                [a],
            )
            .unwrap();
        s.save_read(a, "H", &text, 2, &d()).unwrap();
        let t = &parse_query("PSV-1203")[0];
        let hits = s.search_lines(t, &Scope::default(), 5).unwrap();
        assert_eq!(hits[0].class, MatchClass::Exact);
        assert_eq!(hits[0].source, TextSource::Verified);
        // new content: the correction is kept but no longer applied
        s.save_read(a, "OTHER", &text, 3, &d()).unwrap();
        let hits = s.search_lines(t, &Scope::default(), 5).unwrap();
        assert!(hits.iter().all(|h| h.source != TextSource::Verified));
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn transient_failure_waits_then_gives_up() {
        let s = Store::open_in_memory().unwrap();
        let (id, _) = s.upsert_file(r"\srv\a.pdf", r"\srv", "a.pdf", 1, 1, 1, state::WAITING).unwrap();
        assert!(s.defer(id, "share offline", 1000, 3).unwrap());
        assert!(s.next_to_read(1001).unwrap().is_none(), "waits out the delay");
        assert_eq!(s.next_to_read(1000 + 700).unwrap().unwrap().id, id);
        assert!(s.defer(id, "share offline", 2000, 3).unwrap());
        assert!(!s.defer(id, "share offline", 9000, 3).unwrap());
        assert_eq!(s.file_by_id(id).unwrap().unwrap().read_state, state::FAILED);
    }
}

#[cfg(test)]
mod walk_tests {
    use super::*;

    #[test]
    fn a_subfolder_seen_but_not_listed_is_not_taken_as_listed() {
        let s = Store::open_in_memory().unwrap();
        s.mark_listed(r"\srv\Project", None, 7).unwrap();
        // Listing the root records its subfolder with the same pass...
        s.touch_dir(r"\srv\Project\J100", Some(r"\srv\Project"), 7).unwrap();
        // ...which must not make a resumed walk skip the subfolder's contents.
        assert_eq!(s.listed_in_pass(r"\srv\Project\J100", 7).unwrap(), None);
        assert_eq!(s.listed_in_pass(r"\srv\Project", 7).unwrap(), Some(vec![r"\srv\Project\J100".to_string()]));
        s.mark_listed(r"\srv\Project\J100", Some(r"\srv\Project"), 7).unwrap();
        assert_eq!(s.listed_in_pass(r"\srv\Project\J100", 7).unwrap(), Some(vec![]));
        assert_eq!(s.listed_in_pass(r"\srv\Project\J100", 8).unwrap(), None, "another pass lists it again");
    }

    #[test]
    fn scopes_given_as_network_paths_queue_pdfs_and_count_them() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_file(r"\srv\Project\J1\a.pdf", r"\srv\Project\J1", "a.pdf", 1, 1, 1, state::UNREAD).unwrap();
        s.upsert_file(r"\srv\Project\b.pdf", r"\srv\Project", "b.pdf", 1, 1, 1, state::UNREAD).unwrap();
        s.upsert_file(r"\srv\Projects\c.pdf", r"\srv\Projects", "c.pdf", 1, 1, 1, state::UNREAD).unwrap();
        s.apply_scopes(&[r"\srv\project".to_string()], &[]).unwrap();
        assert_eq!(s.scope_counts(r"\srv\Project").unwrap(), (2, 0, 2, 0));
        assert_eq!(s.count_state(state::UNREAD).unwrap(), 1, "a sibling with a longer name is not inside the folder");
        // Only asked-for files are claimed while the first walk runs.
        assert!(s.claim_urgent(10, "t").unwrap().is_none());
        assert!(s.request_read(r"\srv\Projects\c.pdf").unwrap());
        assert_eq!(s.claim_urgent(10, "t").unwrap().unwrap().name, "c.pdf");
    }
}
