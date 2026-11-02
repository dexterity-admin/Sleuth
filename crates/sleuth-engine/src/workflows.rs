//! Engineering workflows on the index: the tag explorer, revision grouping
//! and comparison, documents sharing tags, and the correction studio.
//!
//! Everything here is a query over what was read, with the evidence attached
//! (page, box, raw reading, confidence). Nothing is inferred: a difference
//! between revisions that could be an OCR misreading is reported as
//! "verify", never as an engineering change.

use crate::store::{Scope, Store, source_code, source_of, state};
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params, params_from_iter};
use sleuth_core::analyzer::{LOW_CONFIDENCE, fold_form, norm_form};
use sleuth_core::paths::DriveMap;
use sleuth_core::protocol::{ChangeKind, DocRev, LowLine, NormBox, Occurrence, Related, RevisionGroup, TagChange, TagSummary, TextSource};
use sleuth_core::settings::RevisionOrder;
use std::collections::HashMap;

/// Tags this close (centre to centre, page fraction) in two revisions are
/// treated as the same place on the sheet.
const SAME_PLACE: f32 = 0.02;

fn bbox(r: &rusqlite::Row, i: usize) -> rusqlite::Result<NormBox> {
    Ok(NormBox { x: r.get::<_, f64>(i)? as f32, y: r.get::<_, f64>(i + 1)? as f32, w: r.get::<_, f64>(i + 2)? as f32, h: r.get::<_, f64>(i + 3)? as f32 })
}

/// Sort key for revisions: A, B, C ... then 0, 1, 2 ... (or the reverse).
pub fn revision_key(rev: Option<&str>, order: RevisionOrder) -> (u8, u32, String) {
    let Some(r) = rev.map(|r| r.trim().to_uppercase()).filter(|r| !r.is_empty()) else { return (9, 0, String::new()) };
    let numeric = r.chars().all(|c| c.is_ascii_digit());
    let group = match (numeric, order) {
        (false, RevisionOrder::LettersThenNumbers) | (true, RevisionOrder::NumbersThenLetters) => 0,
        _ => 1,
    };
    let n = if numeric { r.parse().unwrap_or(0) } else { 0 };
    // "AA" after "Z"
    (group, if numeric { n } else { r.len() as u32 }, r)
}

struct Occ {
    norm: String,
    category: String,
    raw: String,
    page: u32,
    conf: f32,
    bbox: NormBox,
}

impl Store {
    /// Distinct tags, most documents first.
    pub fn tags(&self, category: Option<&str>, contains: Option<&str>, scope: &Scope, limit: usize, offset: usize) -> Result<(Vec<TagSummary>, u64)> {
        let (where_, sp) = scope.sql();
        let mut cond = String::new();
        let mut args: Vec<rusqlite::types::Value> = sp.into_iter().map(Into::into).collect();
        if let Some(c) = category.filter(|c| !c.is_empty()) {
            args.push(c.to_lowercase().into());
            cond.push_str(&format!(" AND t.category=?{}", args.len()));
        }
        if let Some(c) = contains.map(norm_form).filter(|c| !c.is_empty()) {
            args.push(format!("%{c}%").into());
            cond.push_str(&format!(" AND t.norm LIKE ?{}", args.len()));
        }
        let base = format!(
            "FROM tag_occurrences o JOIN engineering_tags t ON t.id=o.tag_id JOIN files f ON f.id=o.file_id WHERE f.read_state={}{where_}{cond}",
            state::READ
        );
        let total: u64 = self.conn.query_row(&format!("SELECT COUNT(DISTINCT t.id) {base}"), params_from_iter(args.clone()), |r| r.get::<_, i64>(0))? as u64;
        args.push((limit as i64).into());
        args.push((offset as i64).into());
        let sql = format!(
            "SELECT t.norm, t.category, COUNT(DISTINCT o.file_id), COUNT(*), MIN(o.conf),
                    (SELECT raw FROM tag_occurrences x WHERE x.tag_id=t.id GROUP BY raw ORDER BY COUNT(*) DESC, MAX(conf) DESC LIMIT 1)
             {base} GROUP BY t.id ORDER BY COUNT(DISTINCT o.file_id) DESC, t.norm LIMIT ?{} OFFSET ?{}",
            args.len() - 1,
            args.len()
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params_from_iter(args), |r| {
            Ok(TagSummary { norm: r.get(0)?, category: r.get(1)?, documents: r.get(2)?, occurrences: r.get(3)?, min_conf: r.get::<_, f64>(4)? as f32, display: r.get(5)? })
        })?;
        Ok((rows.collect::<rusqlite::Result<_>>()?, total))
    }

    /// Every occurrence of one tag.
    pub fn occurrences(&self, norm: &str, scope: &Scope, drives: &DriveMap) -> Result<Vec<Occurrence>> {
        let (where_, sp) = scope.sql();
        let mut args: Vec<rusqlite::types::Value> = sp.into_iter().map(Into::into).collect();
        args.push(norm_form(norm).into());
        let sql = format!(
            "SELECT f.path, f.name, o.page, o.raw, o.conf, o.source, o.x, o.y, o.w, o.h, m.drawing_no, m.revision
             FROM tag_occurrences o JOIN engineering_tags t ON t.id=o.tag_id JOIN files f ON f.id=o.file_id
             LEFT JOIN drawing_metadata m ON m.file_id=f.id
             WHERE f.read_state={}{where_} AND t.norm=?{} ORDER BY f.path, o.page, o.y LIMIT 2000",
            state::READ,
            args.len()
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params_from_iter(args), |r| {
            let path: String = r.get(0)?;
            Ok(Occurrence {
                display_path: drives.to_display(&path),
                path,
                name: r.get(1)?,
                page: r.get(2)?,
                raw: r.get(3)?,
                conf: r.get::<_, f64>(4)? as f32,
                source: source_of(r.get(5)?),
                bbox: bbox(r, 6)?,
                drawing_no: r.get(10)?,
                revision: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drawings with the same drawing number, each with its revisions.
    pub fn revision_groups(&self, contains: Option<&str>, scope: &Scope, order: RevisionOrder, limit: usize, drives: &DriveMap) -> Result<Vec<RevisionGroup>> {
        let (where_, sp) = scope.sql();
        let mut args: Vec<rusqlite::types::Value> = sp.into_iter().map(Into::into).collect();
        let mut cond = String::new();
        if let Some(c) = contains.map(norm_form).filter(|c| !c.is_empty()) {
            args.push(format!("%{c}%").into());
            cond = format!(" AND m.drawing_norm LIKE ?{}", args.len());
        }
        let sql = format!(
            "SELECT m.drawing_norm, m.drawing_no, f.path, f.name, m.revision, m.title, f.mtime
             FROM drawing_metadata m JOIN files f ON f.id=m.file_id
             WHERE f.read_state={} AND m.drawing_norm IS NOT NULL AND m.drawing_norm<>''{where_}{cond}
             ORDER BY m.drawing_norm",
            state::READ
        );
        let mut st = self.conn.prepare(&sql)?;
        let mut groups: Vec<(String, RevisionGroup)> = Vec::new();
        let rows = st.query_map(params_from_iter(args), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, i64>(6)?)))?;
        for row in rows {
            let (norm, no, path, name, revision, title, mtime) = row?;
            if groups.last().is_none_or(|(n, _)| *n != norm) {
                groups.push((norm.clone(), RevisionGroup { drawing_no: no, docs: Vec::new() }));
            }
            groups.last_mut().unwrap().1.docs.push(DocRev { display_path: drives.to_display(&path), path, name, revision, title, modified: mtime });
        }
        let mut out: Vec<RevisionGroup> = groups.into_iter().map(|(_, g)| g).collect();
        for g in &mut out {
            g.docs.sort_by(|a, b| revision_key(a.revision.as_deref(), order).cmp(&revision_key(b.revision.as_deref(), order)).then(a.modified.cmp(&b.modified)));
        }
        // Drawings with more than one issue first: those are worth comparing.
        out.sort_by(|a, b| (b.docs.len() > 1).cmp(&(a.docs.len() > 1)).then(a.drawing_no.cmp(&b.drawing_no)));
        out.truncate(limit);
        Ok(out)
    }

    fn file_occurrences(&self, file_id: i64) -> Result<Vec<Occ>> {
        let mut st = self.conn.prepare_cached(
            "SELECT t.norm, t.category, o.raw, o.page, o.conf, o.x, o.y, o.w, o.h FROM tag_occurrences o JOIN engineering_tags t ON t.id=o.tag_id WHERE o.file_id=?1 ORDER BY o.page, o.y, o.x",
        )?;
        let rows = st.query_map([file_id], |r| Ok(Occ { norm: r.get(0)?, category: r.get(1)?, raw: r.get(2)?, page: r.get(3)?, conf: r.get::<_, f64>(4)? as f32, bbox: bbox(r, 5)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn read_file_id(&self, path: &str) -> Result<i64> {
        match self.file_by_path(path)? {
            Some(f) if f.read_state == state::READ => Ok(f.id),
            Some(_) => bail!("{path} has not been read yet"),
            None => bail!("{path} is not in the index"),
        }
    }

    /// Compare the tags of two revisions. Returns the changes and the number
    /// of tags found unchanged in both.
    pub fn compare(&self, old_path: &str, new_path: &str) -> Result<(Vec<TagChange>, u32)> {
        let (a, b) = (self.read_file_id(old_path)?, self.read_file_id(new_path)?);
        let (old, new) = (self.file_occurrences(a)?, self.file_occurrences(b)?);
        let best = |list: &[Occ]| {
            let mut m: HashMap<String, usize> = HashMap::new();
            for (i, o) in list.iter().enumerate() {
                let e = m.entry(o.norm.clone()).or_insert(i);
                if o.conf > list[*e].conf {
                    *e = i;
                }
            }
            m
        };
        let (bo, bn) = (best(&old), best(&new));
        let mut only_old: Vec<usize> = bo.iter().filter(|(k, _)| !bn.contains_key(*k)).map(|(_, i)| *i).collect();
        let mut only_new: Vec<usize> = bn.iter().filter(|(k, _)| !bo.contains_key(*k)).map(|(_, i)| *i).collect();
        only_old.sort();
        only_new.sort();
        let unchanged = bo.keys().filter(|k| bn.contains_key(*k)).count() as u32;
        let mut changes = Vec::new();
        let mut used_new = vec![false; new.len()];
        let centre = |b: &NormBox| (b.x + b.w / 2.0, b.y + b.h / 2.0);
        for &i in &only_old {
            let o = &old[i];
            // 1. the same characters up to look-alikes: an OCR question, not a change
            let look = only_new.iter().copied().find(|&j| !used_new[j] && fold_form(&new[j].norm) == fold_form(&o.norm));
            if let Some(j) = look {
                used_new[j] = true;
                changes.push(change(ChangeKind::Verify, Some(o), Some(&new[j]), "same characters except look-alikes (O/0, I/1, S/5, B/8); check both pages"));
                continue;
            }
            // 2. a different tag of the same kind in the same place
            let (ox, oy) = centre(&o.bbox);
            let near = only_new.iter().copied().find(|&j| {
                let n = &new[j];
                let (nx, ny) = centre(&n.bbox);
                !used_new[j] && n.page == o.page && n.category == o.category && (nx - ox).abs() < SAME_PLACE && (ny - oy).abs() < SAME_PLACE
            });
            if let Some(j) = near {
                used_new[j] = true;
                let n = &new[j];
                if o.conf < LOW_CONFIDENCE || n.conf < LOW_CONFIDENCE {
                    changes.push(change(ChangeKind::Verify, Some(o), Some(n), "reads differently in the same place, but one reading is low-confidence"));
                } else {
                    changes.push(change(ChangeKind::Changed, Some(o), Some(n), "a different tag of the same kind in the same place"));
                }
                continue;
            }
            if o.conf < LOW_CONFIDENCE {
                changes.push(change(ChangeKind::Verify, Some(o), None, "only in the old revision, and read there with low confidence"));
            } else {
                changes.push(change(ChangeKind::Removed, Some(o), None, "only in the old revision"));
            }
        }
        for &j in &only_new {
            if used_new[j] {
                continue;
            }
            let n = &new[j];
            if n.conf < LOW_CONFIDENCE {
                changes.push(change(ChangeKind::Verify, None, Some(n), "only in the new revision, and read there with low confidence"));
            } else {
                changes.push(change(ChangeKind::Added, None, Some(n), "only in the new revision"));
            }
        }
        changes.sort_by_key(|c| match c.kind {
            ChangeKind::Changed => 0,
            ChangeKind::Added => 1,
            ChangeKind::Removed => 2,
            ChangeKind::Verify => 3,
        });
        Ok((changes, unchanged))
    }

    /// Other read documents sharing tags with this one (not counting
    /// drawing numbers and specs, which link too much).
    pub fn related(&self, path: &str, scope: &Scope, drives: &DriveMap) -> Result<Vec<Related>> {
        let id = self.read_file_id(path)?;
        let (where_, sp) = scope.sql_from(2);
        let mut st = self.conn.prepare(&format!(
            "SELECT f.path, f.name, m.drawing_no, COUNT(DISTINCT t.id), GROUP_CONCAT(DISTINCT t.norm), MIN(o.page)
             FROM tag_occurrences o JOIN engineering_tags t ON t.id=o.tag_id JOIN files f ON f.id=o.file_id
             LEFT JOIN drawing_metadata m ON m.file_id=f.id
             WHERE o.file_id<>?1 AND f.read_state=?2 AND t.category NOT IN ('drawing','spec')
               AND t.id IN (SELECT tag_id FROM tag_occurrences WHERE file_id=?1){where_}
             GROUP BY f.id ORDER BY COUNT(DISTINCT t.id) DESC, f.name LIMIT 100"
        ))?;
        let mut args: Vec<rusqlite::types::Value> = vec![id.into(), state::READ.into()];
        args.extend(sp.into_iter().map(Into::into));
        let rows = st.query_map(params_from_iter(args), |r| {
            let path: String = r.get(0)?;
            let shared: String = r.get::<_, Option<String>>(4)?.unwrap_or_default();
            let mut s: Vec<String> = shared.split(',').map(str::to_string).collect();
            s.sort_by_key(|x| std::cmp::Reverse(x.len()));
            s.truncate(12);
            Ok(Related { display_path: drives.to_display(&path), path, name: r.get(1)?, drawing_no: r.get(2)?, shared_count: r.get(3)?, shared: s, page: r.get(5)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// OCR lines below the confidence threshold or where the two readers
    /// disagreed, worst first.
    pub fn low_confidence(&self, path: Option<&str>, limit: usize, offset: usize, drives: &DriveMap) -> Result<(Vec<LowLine>, u64)> {
        let file_filter = match path {
            Some(p) => Some(self.read_file_id(p)?),
            None => None,
        };
        let cond = "l.source=2 AND l.alt=0 AND (l.conf<?1 OR EXISTS(SELECT 1 FROM lines a WHERE a.file_id=l.file_id AND a.page=l.page AND a.alt=1 AND a.x=l.x AND a.y=l.y))
                    AND f.read_state=?2 AND (?3 IS NULL OR l.file_id=?3)";
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM lines l JOIN files f ON f.id=l.file_id WHERE {cond}"),
            params![LOW_CONFIDENCE as f64, state::READ, file_filter],
            |r| r.get(0),
        )?;
        let mut st = self.conn.prepare(&format!(
            "SELECT l.id, f.path, f.name, l.page, l.text, l.conf, l.x, l.y, l.w, l.h,
                    (SELECT a.text FROM lines a WHERE a.file_id=l.file_id AND a.page=l.page AND a.alt=1 AND a.x=l.x AND a.y=l.y LIMIT 1),
                    (SELECT c.corrected FROM corrections c WHERE c.file_id=l.file_id AND c.page=l.page AND c.stale=0 AND abs(c.x-l.x)<1e-4 AND abs(c.y-l.y)<1e-4 ORDER BY c.id DESC LIMIT 1),
                    (SELECT c.id FROM corrections c WHERE c.file_id=l.file_id AND c.page=l.page AND c.stale=0 AND abs(c.x-l.x)<1e-4 AND abs(c.y-l.y)<1e-4 ORDER BY c.id DESC LIMIT 1)
             FROM lines l JOIN files f ON f.id=l.file_id WHERE {cond} ORDER BY l.conf, f.path, l.page LIMIT ?4 OFFSET ?5"
        ))?;
        let rows = st.query_map(params![LOW_CONFIDENCE as f64, state::READ, file_filter, limit as i64, offset as i64], |r| {
            let path: String = r.get(1)?;
            Ok(LowLine {
                line_id: r.get(0)?,
                display_path: drives.to_display(&path),
                path,
                name: r.get(2)?,
                page: r.get(3)?,
                text: r.get(4)?,
                conf: r.get::<_, f64>(5)? as f32,
                bbox: bbox(r, 6)?,
                alt: r.get(10)?,
                corrected: r.get(11)?,
                correction_id: r.get(12)?,
            })
        })?;
        Ok((rows.collect::<rusqlite::Result<_>>()?, total as u64))
    }

    /// Record a person's reading of one OCR line. The original line stays
    /// (searchable, marked as OCR); the correction is added as a checked line
    /// and the file's tags are derived again. Returns the correction id.
    pub fn correct(&mut self, line_id: i64, text: &str, user: &str, now: i64, deriver: &crate::derive::Deriver) -> Result<i64> {
        let text = text.trim();
        if text.is_empty() {
            bail!("type the text as it appears on the drawing");
        }
        if text.chars().count() > 200 {
            bail!("a correction is one line of up to 200 characters");
        }
        let row: Option<(i64, u32, String, f64, NormBox, Option<String>)> = self
            .conn
            .query_row(
                "SELECT l.file_id, l.page, l.text, l.conf, l.x, l.y, l.w, l.h, f.hash FROM lines l JOIN files f ON f.id=l.file_id WHERE l.id=?1 AND l.source=?2",
                params![line_id, source_code(TextSource::Ocr)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, bbox(r, 4)?, r.get(8)?)),
            )
            .optional()?;
        let Some((file_id, page, original, conf, b, hash)) = row else { bail!("that line is no longer in the index (the file was read again)") };
        let tx = self.conn.transaction()?;
        // One live correction per line: a new one replaces the previous.
        let old: Vec<i64> = {
            let mut st = tx.prepare("SELECT id FROM corrections WHERE file_id=?1 AND page=?2 AND stale=0 AND abs(x-?3)<1e-4 AND abs(y-?4)<1e-4")?;
            st.query_map(params![file_id, page, b.x as f64, b.y as f64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in &old {
            remove_correction(&tx, *id)?;
        }
        tx.execute(
            "INSERT INTO corrections(file_id, file_hash, page, x, y, w, h, original, original_conf, corrected, user, at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![file_id, hash.unwrap_or_default(), page, b.x as f64, b.y as f64, b.w as f64, b.h as f64, original, conf, text, user, now],
        )?;
        let cid = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO lines(file_id, page, text, conf, source, alt, x, y, w, h) VALUES(?1,?2,?3,1.0,?4,0,?5,?6,?7,?8)",
            params![file_id, page, text, source_code(TextSource::Verified), b.x as f64, b.y as f64, b.w as f64, b.h as f64],
        )?;
        tx.execute("INSERT INTO lines_fts(rowid, fold) VALUES(?1, ?2)", params![tx.last_insert_rowid(), fold_form(text)])?;
        tx.commit()?;
        self.rederive(file_id, deriver)?;
        Ok(cid)
    }

    pub fn uncorrect(&mut self, correction_id: i64, deriver: &crate::derive::Deriver) -> Result<()> {
        let file_id: Option<i64> = self.conn.query_row("SELECT file_id FROM corrections WHERE id=?1", [correction_id], |r| r.get(0)).optional()?;
        let Some(file_id) = file_id else { bail!("that correction no longer exists") };
        let tx = self.conn.transaction()?;
        remove_correction(&tx, correction_id)?;
        tx.commit()?;
        self.rederive(file_id, deriver)
    }

    /// Where a line sits, for cropping it in the correction studio.
    pub fn line_place(&self, line_id: i64) -> Result<Option<(String, u32, NormBox)>> {
        Ok(self
            .conn
            .query_row("SELECT f.path, l.page, l.x, l.y, l.w, l.h FROM lines l JOIN files f ON f.id=l.file_id WHERE l.id=?1", [line_id], |r| Ok((r.get(0)?, r.get(1)?, bbox(r, 2)?)))
            .optional()?)
    }
}

fn remove_correction(tx: &rusqlite::Transaction, id: i64) -> Result<()> {
    let row: Option<(i64, u32, f64, f64, String)> =
        tx.query_row("SELECT file_id, page, x, y, corrected FROM corrections WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
    if let Some((file_id, page, x, y, text)) = row {
        let ids: Vec<i64> = {
            let mut st = tx.prepare("SELECT id FROM lines WHERE file_id=?1 AND page=?2 AND source=3 AND abs(x-?3)<1e-4 AND abs(y-?4)<1e-4 AND text=?5")?;
            st.query_map(params![file_id, page, x, y, text], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for lid in ids {
            tx.execute("DELETE FROM lines_fts WHERE rowid=?1", [lid])?;
            tx.execute("DELETE FROM lines WHERE id=?1", [lid])?;
        }
    }
    tx.execute("DELETE FROM corrections WHERE id=?1", [id])?;
    Ok(())
}

fn change(kind: ChangeKind, o: Option<&Occ>, n: Option<&Occ>, reason: &str) -> TagChange {
    TagChange {
        kind,
        category: o.or(n).map(|x| x.category.clone()).unwrap_or_default(),
        old: o.map(|x| x.raw.clone()),
        new: n.map(|x| x.raw.clone()),
        old_page: o.map(|x| x.page),
        new_page: n.map(|x| x.page),
        old_box: o.map(|x| x.bbox),
        new_box: n.map(|x| x.bbox),
        old_conf: o.map(|x| x.conf),
        new_conf: n.map(|x| x.conf),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::Deriver;
    use crate::text::{FileText, Line, PageText};
    use sleuth_core::settings::Settings;

    fn l(text: &str, conf: f32, x: f32, y: f32) -> Line {
        let source = if conf < 1.0 { TextSource::Ocr } else { TextSource::TextLayer };
        Line { text: text.into(), conf, source, bbox: NormBox { x, y, w: 0.08, h: 0.012 }, alt: false }
    }

    fn add(s: &mut Store, path: &str, hash: &str, lines: Vec<Line>) -> i64 {
        let (dir, name) = path.rsplit_once('\\').unwrap();
        let (id, _) = s.upsert_file(path, dir, name, 1, 1, 1, state::WAITING).unwrap();
        s.save_read(id, hash, &FileText { reader_version: 2, model: "m".into(), pages: vec![PageText { page: 1, lines }], ..Default::default() }, 1, &Deriver::new(&Settings::default())).unwrap();
        id
    }

    #[test]
    fn revisions_and_comparison() {
        let mut s = Store::open_in_memory().unwrap();
        add(
            &mut s,
            r"C:\p\ISO-1001-01 rev A.pdf",
            "a",
            vec![l("DRAWING No.: ISO-1001-01", 1.0, 0.6, 0.9), l("REV. A", 1.0, 0.85, 0.9), l("PSV-1203", 1.0, 0.2, 0.2), l("FV-2201", 1.0, 0.4, 0.4), l("HV-4410", 1.0, 0.5, 0.5), l("TV-3301", 0.6, 0.3, 0.6), l("PT-5501", 1.0, 0.7, 0.3)],
        );
        add(
            &mut s,
            r"C:\p\ISO-1001-01 rev B.pdf",
            "b",
            vec![l("DRAWING No.: ISO-1001-01", 1.0, 0.6, 0.9), l("REV. B", 1.0, 0.85, 0.9), l("PSV-1203", 1.0, 0.2, 0.2), l("FV-2202", 1.0, 0.4, 0.4), l("HV-441O", 0.9, 0.5, 0.5), l("LT-6601", 1.0, 0.1, 0.7)],
        );
        let groups = s.revision_groups(None, &Scope::default(), RevisionOrder::LettersThenNumbers, 10, &DriveMap::default()).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].docs.iter().map(|d| d.revision.clone().unwrap()).collect::<Vec<_>>(), vec!["A", "B"]);
        let (changes, unchanged) = s.compare(r"C:\p\ISO-1001-01 rev A.pdf", r"C:\p\ISO-1001-01 rev B.pdf").unwrap();
        assert_eq!(unchanged, 2, "PSV-1203 and the drawing number");
        let kinds: Vec<(ChangeKind, Option<String>, Option<String>)> = changes.iter().map(|c| (c.kind, c.old.clone(), c.new.clone())).collect();
        assert!(kinds.contains(&(ChangeKind::Changed, Some("FV-2201".into()), Some("FV-2202".into()))));
        assert!(kinds.contains(&(ChangeKind::Verify, Some("HV-4410".into()), Some("HV-441O".into()))), "a look-alike is never a change");
        assert!(kinds.contains(&(ChangeKind::Verify, Some("TV-3301".into()), None)), "a low-confidence reading is never a removal");
        assert!(kinds.contains(&(ChangeKind::Removed, Some("PT-5501".into()), None)), "{kinds:?}");
        assert!(kinds.contains(&(ChangeKind::Added, None, Some("LT-6601".into()))));
    }

    #[test]
    fn revision_order() {
        let mut v = vec!["1", "B", "0", "A", "10", "2"];
        v.sort_by_key(|r| revision_key(Some(r), RevisionOrder::LettersThenNumbers));
        assert_eq!(v, vec!["A", "B", "0", "1", "2", "10"]);
    }

    #[test]
    fn explorer_related_and_corrections() {
        let mut s = Store::open_in_memory().unwrap();
        add(&mut s, r"C:\p\pid.pdf", "p", vec![l("PSV-1203", 1.0, 0.1, 0.1), l("LINE 6\"-P-1001-A1B", 1.0, 0.2, 0.2)]);
        add(&mut s, r"C:\p\iso.pdf", "i", vec![l("PSV-12O3", 0.62, 0.1, 0.1), l("6\"-P-1001-A1B", 1.0, 0.3, 0.3)]);
        let (tags, total) = s.tags(None, None, &Scope::default(), 50, 0).unwrap();
        assert_eq!(total, 2, "PSV-12O3 is not a tag: no rule makes O a digit");
        assert_eq!(tags[0].norm, "6P1001A1B");
        assert_eq!(tags[0].documents, 2);
        let rel = s.related(r"C:\p\pid.pdf", &Scope::default(), &DriveMap::default()).unwrap();
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].shared, vec!["6P1001A1B".to_string()]);
        // correction studio
        let (low, n) = s.low_confidence(None, 10, 0, &DriveMap::default()).unwrap();
        assert_eq!(n, 1);
        assert_eq!(low[0].text, "PSV-12O3");
        let d = Deriver::new(&Settings::default());
        let cid = s.correct(low[0].line_id, "PSV-1203", "tester", 5, &d).unwrap();
        let occ = s.occurrences("PSV1203", &Scope::default(), &DriveMap::default()).unwrap();
        assert_eq!(occ.len(), 2, "the corrected reading now counts for the tag");
        assert!(occ.iter().any(|o| o.source == TextSource::Verified));
        assert!(s.occurrences("PSV12O3", &Scope::default(), &DriveMap::default()).unwrap().is_empty(), "the misreading no longer makes a tag");
        let (low, _) = s.low_confidence(None, 10, 0, &DriveMap::default()).unwrap();
        assert_eq!(low[0].corrected.as_deref(), Some("PSV-1203"), "original line kept, with its correction");
        s.uncorrect(cid, &d).unwrap();
        assert_eq!(s.occurrences("PSV1203", &Scope::default(), &DriveMap::default()).unwrap().len(), 1);
    }
}
