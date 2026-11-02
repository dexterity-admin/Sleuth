//! `audit.db`: who did what on the office server, kept apart from the index.
//!
//! Recorded: refused requests (bad key, outside a user's projects, rate
//! limit), administrator actions (scans, cancelled jobs) and changes made
//! with the command line (users, projects, certificates). Each row is time,
//! user, action, target and result.
//!
//! - Keys are never written. Free text (search words, paths) is cut to
//!   [`MAX_TEXT`] characters.
//! - The table keeps the newest [`KEEP`] rows; older ones are dropped as new
//!   ones arrive.

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

pub const AUDIT_FILE: &str = "audit.db";
pub const KEEP: i64 = 100_000;
pub const MAX_TEXT: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub at: i64,
    pub user: String,
    pub action: String,
    pub target: String,
    pub result: String,
}

pub struct Audit {
    conn: Mutex<Connection>,
}

fn cut(s: &str) -> String {
    s.chars().take(MAX_TEXT).collect()
}

impl Audit {
    pub fn open(data: &Path) -> Result<Audit> {
        let conn = Connection::open(data.join(AUDIT_FILE))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS audit(id INTEGER PRIMARY KEY, at INTEGER NOT NULL, user TEXT NOT NULL,
                 action TEXT NOT NULL, target TEXT NOT NULL, result TEXT NOT NULL);",
        )?;
        Ok(Audit { conn: Mutex::new(conn) })
    }

    /// Record one row. A failure is logged, never passed on: auditing must
    /// not stop the server answering.
    pub fn record(&self, user: &str, action: &str, target: &str, result: &str) {
        let at = sleuth_engine::service::now();
        let c = self.conn.lock().unwrap();
        let r = c.execute("INSERT INTO audit(at, user, action, target, result) VALUES (?1, ?2, ?3, ?4, ?5)", params![at, cut(user), cut(action), cut(target), cut(result)]).and_then(|_| {
            let id = c.last_insert_rowid();
            if id % 1000 == 0 { c.execute("DELETE FROM audit WHERE id <= ?1", params![id - KEEP]).map(|_| ()) } else { Ok(()) }
        });
        if let Err(e) = r {
            tracing::warn!("audit write failed: {e}");
        }
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<Row>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT at, user, action, target, result FROM audit ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map([limit as i64], |r| Ok(Row { at: r.get(0)?, user: r.get(1)?, action: r.get(2)?, target: r.get(3)?, result: r.get(4)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drop rows beyond [`KEEP`] now (also done every 1000 inserts).
    pub fn trim(&self) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM audit WHERE id <= (SELECT MAX(id) FROM audit) - ?1", [KEEP])?;
        Ok(())
    }
}

/// The Windows user running a command-line change.
pub fn cli_user() -> String {
    format!("{} (command line)", std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_cut_and_bounded() {
        let dir = std::env::temp_dir().join(format!("sleuth-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = Audit::open(&dir).unwrap();
        a.record("ana", "search", &"x".repeat(500), "forbidden");
        let r = a.recent(10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].target.chars().count(), MAX_TEXT);
        a.record("ana", "search", "y", "ok");
        // the newest row now stands more than KEEP rows after the first
        a.conn.lock().unwrap().execute("UPDATE audit SET id = ?1 WHERE target = 'y'", [KEEP + 10]).unwrap();
        a.trim().unwrap();
        let r = a.recent(10).unwrap();
        assert_eq!(r.len(), 1, "the row more than KEEP behind is gone");
        assert_eq!(r[0].target, "y");
        drop(a);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
