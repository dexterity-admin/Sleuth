//! The local index: one SQLite database in `%LOCALAPPDATA%\Sleuth\index.db`.
//!
//! - `files` / `dirs`: every file and folder seen on the indexed drives, keyed
//!   by UNC path for network locations.
//! - `names`: trigram full-text index over folded file names.
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
use sleuth_core::analyzer::{MatchClass, Term, fold_form, norm_form};
use sleuth_core::protocol::{DrawingMeta, NormBox, PageInfo, Provenance, TextSource};
use std::path::{Path, PathBuf};

/// `PRAGMA user_version`: what Sleuth 0.1 checks. Never raised again.
pub const SCHEMA_VERSION: i64 = 1;
/// Additive schema level in `meta.schema`.
pub const SCHEMA_LEVEL: i64 = 2;
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
}

pub struct Store {
    pub conn: Connection,
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
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        let mut s = Store { conn };
        s.migrate(Some(path))?;
        Ok(s)
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

    pub fn touch_dir(&self, path: &str, parent: Option<&str>, seen: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO dirs(path, parent, seen) VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET seen=excluded.seen",
            params![path, parent, seen],
        )?;
        Ok(())
    }

    /// Subfolders already listed in pass `seen` (resuming an interrupted walk).
    pub fn listed_in_pass(&self, dir: &str, seen: i64) -> Result<Option<Vec<String>>> {
        let done: Option<i64> = self.conn.query_row("SELECT seen FROM dirs WHERE path=?1", [dir], |r| r.get(0)).optional()?;
        if done != Some(seen) {
            return Ok(None);
        }
        let mut st = self.conn.prepare_cached("SELECT path FROM dirs WHERE parent=?1")?;
        let kids = st.query_map([dir], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(Some(kids))
    }

    /// After a folder was listed completely: forget its files and subfolders
    /// that were not seen in this listing. Never called for a listing that
    /// failed or timed out, so an unreachable folder never loses its files.
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

    fn delete_tree(&self, dir: &str, now: i64) -> Result<()> {
        let prefix = format!("{}\\\\%", like_escape(dir.trim_end_matches('\\')));
        let ids: Vec<i64> = {
            let mut st = self.conn.prepare("SELECT id FROM files WHERE (dir=?1 OR dir LIKE ?2 ESCAPE '\\') AND read_state<>?3")?;
            st.query_map(params![dir, prefix, state::GONE], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            self.delete_file(id, now)?;
        }
        self.conn.execute("DELETE FROM dirs WHERE path=?1 OR path LIKE ?2 ESCAPE '\\'", params![dir, prefix])?;
        Ok(())
    }

    /// A file is no longer where it was. A read file keeps its readings for a
    /// while (state `GONE`) so the same content found elsewhere is not read
    /// again; anything else is removed now.
    fn delete_file(&self, id: i64, now: i64) -> Result<()> {
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
        Ok(self
            .conn
            .query_row(
                "UPDATE files SET lease_until=?3, lease_owner=?4 WHERE id=(
                    SELECT id FROM files WHERE read_state=?1 AND (retry_at IS NULL OR retry_at<=?2) AND (lease_until IS NULL OR lease_until<=?2)
                    ORDER BY priority DESC, mtime DESC LIMIT 1)
                 RETURNING id, path, name, size, mtime, read_state",
                params![state::WAITING, now, now + LEASE_SECS, owner],
                file_row,
            )
            .optional()?)
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

    pub fn file_by_id(&self, id: i64) -> Result<Option<FileRow>> {
        Ok(self.conn.query_row("SELECT id, path, name, size, mtime, read_state FROM files WHERE id=?1", [id], file_row).optional()?)
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
        let mut st = self.conn.prepare_cached("SELECT page, class, chars, image_coverage, ocr_tiles, ocr_skipped, ocr_ms, note FROM pages WHERE file_id=?1 ORDER BY page")?;
        let rows = st.query_map([file_id], |r| {
            Ok(PageInfo { page: r.get(0)?, class: r.get(1)?, chars: r.get(2)?, image_coverage: r.get::<_, f64>(3)? as f32, ocr_tiles: r.get(4)?, ocr_tiles_skipped: r.get(5)?, ocr_millis: r.get::<_, i64>(6)? as u64, note: r.get(7)? })
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
            let mut pg = tx.prepare_cached("INSERT OR REPLACE INTO pages(file_id, page, class, chars, image_coverage, ocr_tiles, ocr_skipped, ocr_ms, note) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?;
            for i in &text.page_info {
                pg.execute(params![file_id, i.page, i.class, i.chars, i.image_coverage as f64, i.ocr_tiles, i.ocr_tiles_skipped, i.ocr_millis as i64, i.note])?;
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

    /// Files whose name matches one term, inside `scope`.
    pub fn search_names(&self, term: &Term, scope: &Scope, limit: usize) -> Result<Vec<Candidate>> {
        let (where_, sp) = scope.sql_from(2);
        let gone = format!("f.read_state<>{}", state::GONE);
        let (sql, first) = if term.fold.chars().count() >= 3 {
            (
                format!("SELECT f.id, f.name FROM names JOIN files f ON f.id=names.rowid WHERE names MATCH ?1 AND {gone}{where_} LIMIT ?2"),
                format!("fold : {}", fts_phrase(&term.fold)),
            )
        } else {
            (format!("SELECT f.id, f.name FROM files f WHERE f.name LIKE ?1 ESCAPE '\\' AND {gone}{where_} LIMIT ?2"), format!("%{}%", like_escape(&term.raw)))
        };
        let mut st = self.conn.prepare_cached(&sql)?;
        let mut args: Vec<rusqlite::types::Value> = vec![first.into(), ((limit * 4) as i64).into()];
        args.extend(sp.into_iter().map(Into::into));
        let rows: Vec<(i64, String)> = st.query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for (file_id, name) in rows {
            // Names are exact text; look-alike folding only applies to OCR.
            if let Some(d) = term.explain(&name, 1.0)
                && d.class != MatchClass::Uncertain
            {
                out.push(Candidate { file_id, page: None, text: name, conf: 1.0, source: TextSource::FileName, bbox: None, class: d.class, why: format!("In the file name: {}", d.why.to_lowercase()) });
            }
        }
        out.sort_by_key(|c| c.class);
        out.truncate(limit);
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
            .query_row(
                "SELECT drawing_no, title, revision, project, discipline, sheet, status, evidence FROM drawing_metadata WHERE file_id=?1",
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
        tx.execute("UPDATE files SET read_state=?1 WHERE read_state IN (?2, ?3)", params![state::UNREAD, state::WAITING, state::OFFICE])?;
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
    let m = d.metadata(name, &lines);
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
        assert_eq!(s.get_meta("schema").unwrap().as_deref(), Some("2"));
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
