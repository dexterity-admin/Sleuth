//! The local index: one SQLite database in `%LOCALAPPDATA%\Sleuth\index.db`.
//!
//! - `files` / `dirs`: every file and folder seen on the indexed drives, keyed
//!   by UNC path for network locations.
//! - `names`: trigram full-text index over folded file names.
//! - `lines` + `lines_fts`: every line read from a PDF (text layer or OCR) with
//!   its page and box, and a trigram index over the folded text.
//!
//! Substring search on the folded form finds every candidate, because the
//! exact and separator-free forms are both contained in it; the precise
//! [`MatchClass`] is then decided in Rust by [`Term::classify`].

use crate::text::{FileText, Line};
use anyhow::{Context, Result};
use sleuth_core::analyzer::{MatchClass, Term, fold_form};
use sleuth_core::protocol::{NormBox, TextSource};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;

/// `files.read_state`
pub mod state {
    pub const NOT_PDF: i64 = 0;
    pub const WAITING: i64 = 1;
    pub const READ: i64 = 2;
    pub const FAILED: i64 = 3;
    /// A PDF outside the folders chosen for reading.
    pub const UNREAD: i64 = 4;
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
}

fn source_code(s: TextSource) -> i64 {
    match s {
        TextSource::TextLayer => 1,
        TextSource::Ocr => 2,
        _ => 0,
    }
}

fn source_of(code: i64) -> TextSource {
    match code {
        1 => TextSource::TextLayer,
        2 => TextSource::Ocr,
        _ => TextSource::FileName,
    }
}

/// Quote a string for an FTS5 phrase query.
fn fts_phrase(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open index {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        let mut s = Store { conn };
        s.migrate()?;
        Ok(s)
    }

    pub fn open_in_memory() -> Result<Store> {
        let mut s = Store { conn: Connection::open_in_memory()? };
        s.migrate()?;
        Ok(s)
    }

    fn migrate(&mut self) -> Result<()> {
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
        Ok(())
    }

    // ---- file names -------------------------------------------------------

    /// Insert or refresh one file seen by a walk. Returns `(id, changed)`;
    /// `changed` is true for new files and files whose size or date moved.
    pub fn upsert_file(&self, path: &str, dir: &str, name: &str, size: u64, mtime: i64, seen: i64, is_pdf_in_scope: Option<bool>) -> Result<(i64, bool)> {
        let existing: Option<(i64, i64, i64, i64)> = self
            .conn
            .query_row("SELECT id, size, mtime, read_state FROM files WHERE path=?1", [path], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let wanted_state = match is_pdf_in_scope {
            None => state::NOT_PDF,
            Some(true) => state::WAITING,
            Some(false) => state::UNREAD,
        };
        match existing {
            Some((id, osize, omtime, ostate)) => {
                let changed = osize as u64 != size || omtime != mtime;
                let new_state = if changed {
                    if ostate == state::READ || ostate == state::FAILED { self.clear_lines(id)?; }
                    wanted_state
                } else if (ostate == state::UNREAD && wanted_state == state::WAITING)
                    || (ostate == state::WAITING && wanted_state == state::UNREAD)
                {
                    wanted_state
                } else {
                    ostate
                };
                self.conn.execute(
                    "UPDATE files SET size=?2, mtime=?3, seen=?4, read_state=?5 WHERE id=?1",
                    params![id, size as i64, mtime, seen, new_state],
                )?;
                Ok((id, changed))
            }
            None => {
                self.conn.execute(
                    "INSERT INTO files(path, dir, name, size, mtime, seen, read_state) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![path, dir, name, size as i64, mtime, seen, wanted_state],
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

    /// After a folder was listed completely: forget its files and subfolders
    /// that were not seen in this listing.
    pub fn reconcile_dir(&self, dir: &str, seen: i64) -> Result<()> {
        let gone: Vec<i64> = {
            let mut st = self.conn.prepare_cached("SELECT id FROM files WHERE dir=?1 AND seen<?2")?;
            st.query_map(params![dir, seen], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in gone {
            self.delete_file(id)?;
        }
        let gone_dirs: Vec<String> = {
            let mut st = self.conn.prepare_cached("SELECT path FROM dirs WHERE parent=?1 AND seen<?2")?;
            st.query_map(params![dir, seen], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for d in gone_dirs {
            self.delete_tree(&d)?;
        }
        Ok(())
    }

    fn delete_tree(&self, dir: &str) -> Result<()> {
        let prefix = format!("{}\\%", dir.trim_end_matches('\\').replace('%', "\\%").replace('_', "\\_"));
        let ids: Vec<i64> = {
            let mut st = self.conn.prepare("SELECT id FROM files WHERE dir=?1 OR dir LIKE ?2 ESCAPE '\\'")?;
            st.query_map(params![dir, prefix], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            self.delete_file(id)?;
        }
        self.conn.execute("DELETE FROM dirs WHERE path=?1 OR path LIKE ?2 ESCAPE '\\'", params![dir, prefix])?;
        Ok(())
    }

    fn delete_file(&self, id: i64) -> Result<()> {
        self.clear_lines(id)?;
        self.conn.execute("DELETE FROM names WHERE rowid=?1", [id])?;
        self.conn.execute("DELETE FROM files WHERE id=?1", [id])?;
        Ok(())
    }

    fn clear_lines(&self, file_id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM lines_fts WHERE rowid IN (SELECT id FROM lines WHERE file_id=?1)", [file_id])?;
        self.conn.execute("DELETE FROM lines WHERE file_id=?1", [file_id])?;
        Ok(())
    }

    // ---- reading queue ----------------------------------------------------

    /// Next PDF to read: requested ones first, then newest. Files waiting
    /// out a retry delay are skipped until `now` reaches it.
    pub fn next_to_read(&self, now: i64) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, name, size, mtime, read_state FROM files WHERE read_state=?1 AND (retry_at IS NULL OR retry_at<=?2) ORDER BY priority DESC, mtime DESC LIMIT 1",
                params![state::WAITING, now],
                |r| Ok(FileRow { id: r.get(0)?, path: r.get(1)?, name: r.get(2)?, size: r.get::<_, i64>(3)? as u64, mtime: r.get(4)?, read_state: r.get(5)? }),
            )
            .optional()?)
    }

    /// Queue a file for reading ahead of everything else.
    pub fn request_read(&self, path: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE files SET read_state=?2, priority=1, attempts=0, retry_at=NULL WHERE path=?1 AND read_state IN (?3, ?4, ?2)",
            params![path, state::WAITING, state::UNREAD, state::FAILED],
        )?;
        Ok(n > 0)
    }

    pub fn file_by_path(&self, path: &str) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row("SELECT id, path, name, size, mtime, read_state FROM files WHERE path=?1", [path], |r| {
                Ok(FileRow { id: r.get(0)?, path: r.get(1)?, name: r.get(2)?, size: r.get::<_, i64>(3)? as u64, mtime: r.get(4)?, read_state: r.get(5)? })
            })
            .optional()?)
    }

    pub fn file_by_id(&self, id: i64) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row("SELECT id, path, name, size, mtime, read_state FROM files WHERE id=?1", [id], |r| {
                Ok(FileRow { id: r.get(0)?, path: r.get(1)?, name: r.get(2)?, size: r.get::<_, i64>(3)? as u64, mtime: r.get(4)?, read_state: r.get(5)? })
            })
            .optional()?)
    }

    /// Lines of a previously read file with the same content, if any.
    pub fn read_copy_by_hash(&self, hash: &str, except: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM files WHERE hash=?1 AND read_state=?2 AND id<>?3 LIMIT 1", params![hash, state::READ, except], |r| r.get(0))
            .optional()?)
    }

    pub fn file_text(&self, file_id: i64) -> Result<FileText> {
        let mut st = self.conn.prepare("SELECT page, text, conf, source, alt, x, y, w, h FROM lines WHERE file_id=?1 ORDER BY page, id")?;
        let mut ft = FileText::default();
        let rows = st.query_map([file_id], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                Line {
                    text: r.get(1)?,
                    conf: r.get::<_, f64>(2)? as f32,
                    source: source_of(r.get(3)?),
                    alt: r.get::<_, i64>(4)? != 0,
                    bbox: NormBox { x: r.get::<_, f64>(5)? as f32, y: r.get::<_, f64>(6)? as f32, w: r.get::<_, f64>(7)? as f32, h: r.get::<_, f64>(8)? as f32 },
                },
            ))
        })?;
        for row in rows {
            let (page, line) = row?;
            if ft.pages.last().is_none_or(|p| p.page != page) {
                ft.pages.push(crate::text::PageText { page, lines: Vec::new() });
            }
            ft.pages.last_mut().unwrap().lines.push(line);
        }
        Ok(ft)
    }

    pub fn save_read(&mut self, file_id: i64, hash: &str, text: &FileText, now: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM lines_fts WHERE rowid IN (SELECT id FROM lines WHERE file_id=?1)", [file_id])?;
        tx.execute("DELETE FROM lines WHERE file_id=?1", [file_id])?;
        {
            let mut ins = tx.prepare_cached("INSERT INTO lines(file_id, page, text, conf, source, alt, x, y, w, h) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)")?;
            let mut fts = tx.prepare_cached("INSERT INTO lines_fts(rowid, fold) VALUES(?1, ?2)")?;
            for p in &text.pages {
                for l in &p.lines {
                    ins.execute(params![file_id, p.page, l.text, l.conf as f64, source_code(l.source), l.alt as i64, l.bbox.x as f64, l.bbox.y as f64, l.bbox.w as f64, l.bbox.h as f64])?;
                    fts.execute(params![tx.last_insert_rowid(), fold_form(&l.text)])?;
                }
            }
        }
        tx.execute(
            "UPDATE files SET read_state=?2, hash=?3, pages=?4, model=?5, read_at=?6, read_error=NULL, priority=0, attempts=0, retry_at=NULL WHERE id=?1",
            params![file_id, state::READ, hash, text.pages.len() as i64, text.model, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A temporary problem (network, file locked): try again later, with a
    /// growing delay. After `max_attempts` the file is marked failed.
    pub fn defer(&self, file_id: i64, reason: &str, now: i64, max_attempts: i64) -> Result<bool> {
        let attempts: i64 = self.conn.query_row("SELECT attempts FROM files WHERE id=?1", [file_id], |r| r.get::<_, i64>(0))? + 1;
        if attempts >= max_attempts {
            self.save_failure(file_id, reason, now)?;
            return Ok(false);
        }
        let delay = 300 * (1i64 << attempts.min(6));
        self.conn.execute(
            "UPDATE files SET attempts=?2, retry_at=?3, read_error=?4 WHERE id=?1",
            params![file_id, attempts, now + delay, reason],
        )?;
        Ok(true)
    }

    pub fn save_failure(&self, file_id: i64, reason: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET read_state=?2, read_error=?3, read_at=?4, priority=0 WHERE id=?1",
            params![file_id, state::FAILED, reason, now],
        )?;
        Ok(())
    }

    /// Put a file back in the queue (e.g. after the reader was stopped midway).
    pub fn requeue(&self, file_id: i64) -> Result<()> {
        self.conn.execute("UPDATE files SET read_state=?2 WHERE id=?1", params![file_id, state::WAITING])?;
        Ok(())
    }

    // ---- search -----------------------------------------------------------

    /// Matching lines for one term, best first, at most `limit`.
    pub fn search_lines(&self, term: &Term, limit: usize) -> Result<Vec<Candidate>> {
        if term.fold.chars().count() < 3 {
            return Ok(Vec::new());
        }
        let mut st = self.conn.prepare_cached(
            "SELECT l.file_id, l.page, l.text, l.conf, l.source, l.x, l.y, l.w, l.h
             FROM lines_fts f JOIN lines l ON l.id=f.rowid WHERE lines_fts MATCH ?1 LIMIT ?2",
        )?;
        let q = format!("fold : {}", fts_phrase(&term.fold));
        let rows = st.query_map(params![q, (limit * 4) as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3)? as f32,
                r.get::<_, i64>(4)?,
                NormBox { x: r.get::<_, f64>(5)? as f32, y: r.get::<_, f64>(6)? as f32, w: r.get::<_, f64>(7)? as f32, h: r.get::<_, f64>(8)? as f32 },
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (file_id, page, text, conf, source, bbox) = row?;
            if let Some(class) = term.classify(&text, conf) {
                out.push(Candidate { file_id, page: Some(page), text, conf, source: source_of(source), bbox: Some(bbox), class });
            }
        }
        out.sort_by_key(|c| c.class);
        out.truncate(limit);
        Ok(out)
    }

    /// Files whose name matches one term.
    pub fn search_names(&self, term: &Term, limit: usize) -> Result<Vec<Candidate>> {
        let rows: Vec<(i64, String)> = if term.fold.chars().count() >= 3 {
            let mut st = self.conn.prepare_cached(
                "SELECT f.id, f.name FROM names n JOIN files f ON f.id=n.rowid WHERE names MATCH ?1 LIMIT ?2",
            )?;
            let q = format!("fold : {}", fts_phrase(&term.fold));
            st.query_map(params![q, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        } else {
            let mut st = self.conn.prepare_cached("SELECT id, name FROM files WHERE name LIKE ?1 LIMIT ?2")?;
            st.query_map(params![format!("%{}%", term.raw), (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for (file_id, name) in rows {
            // Names are exact text; look-alike folding only applies to OCR.
            if let Some(class) = term.classify(&name, 1.0)
                && class != MatchClass::Uncertain
            {
                out.push(Candidate { file_id, page: None, text: name, conf: 1.0, source: TextSource::FileName, bbox: None, class });
            }
        }
        out.sort_by_key(|c| c.class);
        out.truncate(limit);
        Ok(out)
    }

    /// Lines on one page of a file that match all terms (for preview boxes).
    pub fn page_boxes(&self, file_id: i64, page: u32, terms: &[Term]) -> Result<Vec<NormBox>> {
        let mut st = self.conn.prepare_cached("SELECT text, conf, x, y, w, h FROM lines WHERE file_id=?1 AND page=?2")?;
        let rows = st.query_map(params![file_id, page], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)? as f32, NormBox { x: r.get::<_, f64>(2)? as f32, y: r.get::<_, f64>(3)? as f32, w: r.get::<_, f64>(4)? as f32, h: r.get::<_, f64>(5)? as f32 }))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (text, conf, b) = row?;
            if terms.iter().any(|t| t.classify(&text, conf).is_some()) {
                out.push(b);
            }
        }
        Ok(out)
    }

    // ---- status -----------------------------------------------------------

    pub fn counts(&self) -> Result<(u64, u64, u64, u64)> {
        let mut st = self.conn.prepare_cached("SELECT read_state, COUNT(*) FROM files GROUP BY read_state")?;
        let (mut all, mut read, mut waiting, mut failed) = (0u64, 0u64, 0u64, 0u64);
        for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64)))? {
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

    pub fn failures(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT path, COALESCE(read_error,'') FROM files WHERE read_state=?1 ORDER BY read_at DESC LIMIT ?2")?;
        let rows = st.query_map(params![state::FAILED, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Mark PDFs under the given folders as waiting and the others as unread.
    pub fn apply_scopes(&self, scopes: &[String]) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET read_state=?1 WHERE read_state=?2",
            params![state::UNREAD, state::WAITING],
        )?;
        for s in scopes {
            let s = s.trim_end_matches('\\');
            let prefix = format!("{}\\%", s.replace('%', "\\%").replace('_', "\\_"));
            self.conn.execute(
                "UPDATE files SET read_state=?1 WHERE read_state=?2 AND (dir=?3 OR dir LIKE ?4 ESCAPE '\\')",
                params![state::WAITING, state::UNREAD, s, prefix],
            )?;
        }
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO meta(key, value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::PageText;
    use sleuth_core::analyzer::parse_query;

    fn line(text: &str, conf: f32, source: TextSource) -> Line {
        Line { text: text.into(), conf, source, bbox: NormBox { x: 0.1, y: 0.2, w: 0.1, h: 0.01 }, alt: false }
    }

    #[test]
    fn content_search_classes() {
        let mut s = Store::open_in_memory().unwrap();
        let (a, _) = s.upsert_file(r"\\srv\p\iso1.pdf", r"\\srv\p", "iso1.pdf", 10, 1, 1, Some(true)).unwrap();
        let (b, _) = s.upsert_file(r"\\srv\p\scan.pdf", r"\\srv\p", "scan.pdf", 10, 1, 1, Some(true)).unwrap();
        s.save_read(a, "h1", &FileText { reader_version: 1, model: "m".into(), pages: vec![PageText { page: 1, lines: vec![line("LINE 10\"-P-1001-A1B CS", 1.0, TextSource::TextLayer)] }] }, 5).unwrap();
        s.save_read(b, "h2", &FileText { reader_version: 1, model: "m".into(), pages: vec![PageText { page: 2, lines: vec![line("10\"-P-l001-A1B", 0.95, TextSource::Ocr)] }] }, 5).unwrap();
        let t = &parse_query("10\"-P-1001-A1B")[0];
        let hits = s.search_lines(t, 50).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].class, MatchClass::Exact);
        assert_eq!(hits[1].class, MatchClass::Uncertain);
        assert_eq!(hits[1].page, Some(2));
    }

    #[test]
    fn names_and_reconcile() {
        let s = Store::open_in_memory().unwrap();
        s.touch_dir(r"C:\d", None, 1).unwrap();
        s.upsert_file(r"C:\d\PSV-1203 datasheet.pdf", r"C:\d", "PSV-1203 datasheet.pdf", 1, 1, 1, Some(false)).unwrap();
        s.upsert_file(r"C:\d\old.pdf", r"C:\d", "old.pdf", 1, 1, 1, Some(false)).unwrap();
        let t = &parse_query("psv1203")[0];
        assert_eq!(s.search_names(t, 10).unwrap()[0].class, MatchClass::SeparatorsIgnored);
        // second walk sees only the datasheet
        s.upsert_file(r"C:\d\PSV-1203 datasheet.pdf", r"C:\d", "PSV-1203 datasheet.pdf", 1, 1, 2, Some(false)).unwrap();
        s.reconcile_dir(r"C:\d", 2).unwrap();
        assert!(s.file_by_path(r"C:\d\old.pdf").unwrap().is_none());
        assert!(s.search_names(&parse_query("old")[0], 10).unwrap().is_empty());
    }

    #[test]
    fn changed_file_is_requeued() {
        let mut s = Store::open_in_memory().unwrap();
        let (a, _) = s.upsert_file(r"C:\a.pdf", r"C:", "a.pdf", 10, 1, 1, Some(true)).unwrap();
        s.save_read(a, "h", &FileText::default(), 1).unwrap();
        let (_, changed) = s.upsert_file(r"C:\a.pdf", r"C:", "a.pdf", 11, 2, 2, Some(true)).unwrap();
        assert!(changed);
        assert_eq!(s.file_by_id(a).unwrap().unwrap().read_state, state::WAITING);
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn transient_failure_waits_then_gives_up() {
        let s = Store::open_in_memory().unwrap();
        let (id, _) = s.upsert_file(r"\srv\a.pdf", r"\srv", "a.pdf", 1, 1, 1, Some(true)).unwrap();
        assert!(s.defer(id, "share offline", 1000, 3).unwrap());
        assert!(s.next_to_read(1001).unwrap().is_none(), "waits out the delay");
        assert_eq!(s.next_to_read(1000 + 700).unwrap().unwrap().id, id);
        assert!(s.defer(id, "share offline", 2000, 3).unwrap());
        assert!(!s.defer(id, "share offline", 9000, 3).unwrap());
        assert_eq!(s.file_by_id(id).unwrap().unwrap().read_state, state::FAILED);
    }
}
