//! The tag, revision, related-document, correction and monitor pages.
//!
//! Each is a query over what was read. Read documents live in several parts
//! (see `pool.rs`), so every query runs on each part and the answers are
//! merged here. Ids that travel to the window and come back (a line, a
//! correction, a document) carry their part with them (`pool::compose`).

use crate::pool::{Covered, compose, split};
use crate::service::{Engine, now};
use crate::store::{Scope, Store, state};
use crate::workflows::{compare_occurrences, revision_key};
use anyhow::{Result, anyhow, bail};
use sleuth_core::analyzer::norm_form;
use sleuth_core::protocol::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

impl Engine {
    fn scope_for(&self, project: Option<&str>) -> Result<Scope> {
        Ok(match project.filter(|p| !p.trim().is_empty()) {
            Some(p) => Scope::roots(&self.project_roots(p)?),
            None => Scope::default(),
        })
    }

    pub fn tags(&self, category: Option<&str>, contains: Option<&str>, project: Option<&str>, limit: usize, offset: usize, office: bool) -> Result<Event> {
        if office {
            let (tags, total) = self.office()?.tags(category, contains, project, limit, offset)?;
            return Ok(Event::Tags { tags, total });
        }
        let (tags, total) = self.tags_in(category, contains, &self.scope_for(project)?, limit, offset)?;
        Ok(Event::Tags { tags, total })
    }

    /// Distinct tags inside `scope`, most documents first.
    pub fn tags_in(&self, category: Option<&str>, contains: Option<&str>, scope: &Scope, limit: usize, offset: usize) -> Result<(Vec<TagSummary>, u64)> {
        // Each part's most common tags; the same tag in several parts adds up.
        let mut merged: HashMap<String, (TagSummary, u32)> = HashMap::new();
        for (_, (tags, _)) in self.pool.each(|_, s| s.tags(category, contains, scope, 5000, 0)) {
            for t in tags {
                match merged.get_mut(&t.norm) {
                    Some((m, most)) => {
                        m.documents += t.documents;
                        m.occurrences += t.occurrences;
                        m.min_conf = m.min_conf.min(t.min_conf);
                        if t.occurrences > *most {
                            *most = t.occurrences;
                            m.display = t.display;
                        }
                    }
                    None => {
                        let n = t.occurrences;
                        merged.insert(t.norm.clone(), (t, n));
                    }
                }
            }
        }
        let mut tags: Vec<TagSummary> = merged.into_values().map(|(t, _)| t).collect();
        tags.sort_by(|a, b| b.documents.cmp(&a.documents).then(a.norm.cmp(&b.norm)));
        let total = tags.len() as u64;
        Ok((tags.into_iter().skip(offset).take(limit.min(2000)).collect(), total))
    }

    pub fn occurrences(&self, norm: &str, project: Option<&str>, office: bool) -> Result<Event> {
        if office {
            return Ok(Event::Occurrences { items: self.office()?.occurrences(norm, project)? });
        }
        Ok(Event::Occurrences { items: self.occurrences_in(norm, &self.scope_for(project)?)? })
    }

    /// Every place one tag was found inside `scope`.
    pub fn occurrences_in(&self, norm: &str, scope: &Scope) -> Result<Vec<Occurrence>> {
        let drives = self.drives.read().unwrap().clone();
        let mut items: Vec<Occurrence> = self.pool.each(|_, s| s.occurrences(norm, scope, &drives)).into_iter().flat_map(|(_, v)| v).collect();
        items.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()).then(a.page.cmp(&b.page)).then(a.bbox.y.total_cmp(&b.bbox.y)));
        items.truncate(2000);
        Ok(items)
    }

    pub fn revisions(&self, contains: Option<&str>, project: Option<&str>, limit: usize, office: bool) -> Result<Event> {
        if office {
            return Ok(Event::Revisions { groups: self.office()?.revisions(contains, project, limit)? });
        }
        Ok(Event::Revisions { groups: self.revisions_in(contains, &self.scope_for(project)?, limit)? })
    }

    /// Drawings inside `scope` grouped by drawing number, with their revisions.
    pub fn revisions_in(&self, contains: Option<&str>, scope: &Scope, limit: usize) -> Result<Vec<RevisionGroup>> {
        let drives = self.drives.read().unwrap().clone();
        let order = self.settings.read().unwrap().revision_order;
        // Revisions of one drawing may sit in different parts.
        let mut by_number: HashMap<String, RevisionGroup> = HashMap::new();
        for (_, groups) in self.pool.each(|_, s| s.revision_groups(contains, scope, order, 5000, &drives)) {
            for g in groups {
                match by_number.get_mut(&norm_form(&g.drawing_no)) {
                    Some(m) => m.docs.extend(g.docs),
                    None => {
                        by_number.insert(norm_form(&g.drawing_no), g);
                    }
                }
            }
        }
        let mut groups: Vec<RevisionGroup> = by_number.into_values().collect();
        for g in &mut groups {
            g.docs.sort_by(|a, b| revision_key(a.revision.as_deref(), order).cmp(&revision_key(b.revision.as_deref(), order)).then(a.modified.cmp(&b.modified)));
        }
        // Drawings with more than one issue first: those are worth comparing.
        groups.sort_by(|a, b| (b.docs.len() > 1).cmp(&(a.docs.len() > 1)).then(a.drawing_no.cmp(&b.drawing_no)));
        groups.truncate(limit.min(2000));
        Ok(groups)
    }

    pub fn compare(&self, old: &str, new: &str, office: bool) -> Result<Event> {
        if office {
            let c = self.office()?;
            let (a, b) = (self.office_doc(&c, old)?, self.office_doc(&c, new)?);
            let r = c.compare(a, b)?;
            return Ok(Event::Comparison { old: r.old, new: r.new, changes: r.changes, unchanged: r.unchanged });
        }
        let (old, new, changes, unchanged) = self.compare_paths(old, new)?;
        Ok(Event::Comparison { old, new, changes, unchanged })
    }

    /// Compare the tags of two read documents: their title-block details,
    /// the changes, and how many tags are unchanged.
    pub fn compare_paths(&self, old: &str, new: &str) -> Result<(DrawingMeta, DrawingMeta, Vec<TagChange>, u32)> {
        let side = |path: &str| -> Result<(Vec<crate::workflows::Occ>, DrawingMeta)> {
            let stored = self.stored_path(path);
            self.pool
                .with_file(&stored, |s, id| {
                    if s.file_by_id(id)?.is_none_or(|f| f.read_state != state::READ) {
                        bail!("{path} has not been read yet");
                    }
                    Ok((s.file_occurrences(id)?, s.metadata(id)?))
                })?
                .ok_or_else(|| anyhow!("{path} has not been read yet"))
        };
        let (old_tags, old_meta) = side(old)?;
        let (new_tags, new_meta) = side(new)?;
        let (changes, unchanged) = compare_occurrences(&old_tags, &new_tags);
        Ok((old_meta, new_meta, changes, unchanged))
    }

    pub fn related(&self, path: &str, office: bool) -> Result<Event> {
        if office {
            let c = self.office()?;
            let id = self.office_doc(&c, path)?;
            return Ok(Event::Related { items: c.related(id)? });
        }
        Ok(Event::Related { items: self.related_in(path, &Scope::default())? })
    }

    /// Other read documents inside `scope` sharing tags with this one.
    pub fn related_in(&self, path: &str, scope: &Scope) -> Result<Vec<Related>> {
        let stored = self.stored_path(path);
        let drives = self.drives.read().unwrap().clone();
        let norms = self.pool.with_file(&stored, |s, id| s.linking_tags(id))?.ok_or_else(|| anyhow!("{path} has not been read yet"))?;
        if norms.is_empty() {
            return Ok(Vec::new());
        }
        let mut items: Vec<Related> = Vec::new();
        for (_, rows) in self.pool.each(|_, s| s.files_with_tags(&norms, scope)) {
            for (p, name, drawing_no, mut shared, page) in rows {
                if p.eq_ignore_ascii_case(&stored) {
                    continue;
                }
                let shared_count = shared.len() as u32;
                // Most specific (longest) first.
                shared.sort_by_key(|x| std::cmp::Reverse(x.len()));
                shared.truncate(12);
                items.push(Related { display_path: drives.to_display(&p), path: p, name, drawing_no, shared, shared_count, page: Some(page) });
            }
        }
        items.sort_by(|a, b| b.shared_count.cmp(&a.shared_count).then(a.name.cmp(&b.name)));
        items.truncate(100);
        Ok(items)
    }

    pub fn doc_info(&self, path: &str, office: bool) -> Result<Event> {
        if office {
            let c = self.office()?;
            let id = self.office_doc(&c, path)?;
            let d = c.document(id)?;
            return Ok(Event::DocInfo { metadata: d.metadata, pages: d.pages, provenance: d.provenance, tags: d.tags });
        }
        let (metadata, pages, provenance, tags) = self.doc_detail(path)?;
        Ok(Event::DocInfo { metadata, pages, provenance, tags })
    }

    /// Title-block details, page classes, how a document was read, and how
    /// many tags it carries.
    pub fn doc_detail(&self, path: &str) -> Result<(DrawingMeta, Vec<PageInfo>, Provenance, u32)> {
        let stored = self.stored_path(path);
        self.pool
            .with_file(&stored, |s, id| {
                let tags: u32 = s.conn.query_row("SELECT COUNT(*) FROM tag_occurrences WHERE file_id=?1", [id], |r| r.get(0))?;
                Ok((s.metadata(id)?, s.page_info(id)?, s.provenance(id)?, tags))
            })?
            .ok_or_else(|| anyhow!("not read yet"))
    }

    /// Every tag found in one document: `(tag, category, as read, page,
    /// confidence, source, box)`.
    #[allow(clippy::type_complexity)]
    pub fn doc_tags(&self, path: &str) -> Result<Vec<(String, String, String, u32, f32, TextSource, NormBox)>> {
        let stored = self.stored_path(path);
        self.pool
            .with_file(&stored, |s, id| {
                let mut q = s.conn.prepare(
                    "SELECT t.norm, t.category, o.raw, o.page, o.conf, o.source, o.x, o.y, o.w, o.h
                     FROM tag_occurrences o JOIN engineering_tags t ON t.id=o.tag_id
                     WHERE o.file_id=?1 ORDER BY o.page, o.y, o.x LIMIT 5000",
                )?;
                let rows = q.query_map([id], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get::<_, f64>(4)? as f32,
                        crate::store::source_of(r.get(5)?),
                        NormBox { x: r.get::<_, f64>(6)? as f32, y: r.get::<_, f64>(7)? as f32, w: r.get::<_, f64>(8)? as f32, h: r.get::<_, f64>(9)? as f32 },
                    ))
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })?
            .ok_or_else(|| anyhow!("not read yet"))
    }

    pub fn low_confidence(&self, path: Option<&str>, limit: usize, offset: usize) -> Result<Event> {
        let drives = self.drives.read().unwrap().clone();
        let limit = limit.min(500);
        let stamp = |slot: u32, mut l: LowLine| {
            l.line_id = compose(slot, l.line_id);
            l.correction_id = l.correction_id.map(|c| compose(slot, c));
            l
        };
        if let Some(p) = path {
            // One file: only the part holding its current reading.
            let stored = self.stored_path(p);
            let Some(c) = self.pool.covered(&stored) else { bail!("{p} has not been read yet") };
            let Some(part) = self.pool.part(c.slot) else { bail!("{p} has not been read yet") };
            let (items, total) = part.with(|s| s.low_confidence(Some(&stored), limit, offset, &drives))?;
            return Ok(Event::LowConfidence { items: items.into_iter().map(|l| stamp(part.slot, l)).collect(), total });
        }
        let mut all: Vec<LowLine> = Vec::new();
        let mut total = 0u64;
        for (part, (items, n)) in self.pool.each(|_, s| s.low_confidence(None, limit + offset, 0, &drives)) {
            total += n;
            all.extend(items.into_iter().map(|l| stamp(part.slot, l)));
        }
        all.sort_by(|a, b| a.conf.total_cmp(&b.conf).then(a.path.cmp(&b.path)).then(a.page.cmp(&b.page)));
        Ok(Event::LowConfidence { items: all.into_iter().skip(offset).take(limit).collect(), total })
    }

    /// Which of this PC's two unpublished-readings files a path belongs in.
    fn work_path(&self, stored: &str) -> PathBuf {
        self.data.join(if stored.starts_with("\\\\") { "work.db" } else { "work-local.db" })
    }

    /// A reading as it stands in this PC's unpublished readings, copied
    /// there from its published part when need be: published parts never
    /// change, so a correction is a new reading that replaces the old one.
    fn editable(&self, slot: u32, file_id: i64, stored: &str) -> Result<(Store, std::sync::Arc<crate::pool::Part>, i64)> {
        let part = self.pool.part(slot).ok_or_else(|| anyhow!("that reading is no longer in the index (the file was read again)"))?;
        let work_path = self.work_path(stored);
        let work_part = self.pool.add_work(&work_path)?;
        let mut work = Store::open(&work_path)?;
        if part.slot == work_part.slot {
            return Ok((work, work_part, file_id));
        }
        let deriver = self.deriver();
        let id = part.with(|s| work.import_reading(s, file_id, &deriver))?;
        Ok((work, work_part, id))
    }

    /// After a correction changed a reading in `work`: it is the current
    /// reading from now on, and is published with the next part.
    fn corrected(&self, work: &Store, work_part: &crate::pool::Part, id: i64, stored: &str) -> Result<()> {
        let t = now();
        work.conn.execute("UPDATE files SET read_at=?2 WHERE id=?1", rusqlite::params![id, t])?;
        let (size, mtime): (i64, i64) = work.conn.query_row("SELECT size, mtime FROM files WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        self.pool.note_saved(work_part, stored, Covered { slot: work_part.slot, id, size: size as u64, mtime, state: state::READ, read_at: t });
        self.pool.recover(&|p| self.presence(p));
        self.work_dirty.store(true, Ordering::SeqCst);
        self.reader_signal.raise();
        Ok(())
    }

    pub fn correct(&self, line_id: i64, text: &str) -> Result<Event> {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        let (slot, local) = split(line_id);
        let part = self.pool.part(slot).ok_or_else(|| anyhow!("that line is no longer in the index (the file was read again)"))?;
        let Some((file_id, stored, page, bbox, line_text)) = part.with(|s| s.line_file(local))? else { bail!("that line is no longer in the index (the file was read again)") };
        let _gate = self.work_gate.lock().unwrap();
        let (mut work, work_part, id) = self.editable(slot, file_id, &stored)?;
        let line = if part.slot == work_part.slot { local } else { work.find_line(id, page, bbox, &line_text)?.ok_or_else(|| anyhow!("that line could not be found again; open the page and try once more"))? };
        let cid = work.correct(line, text, &user, now(), &self.deriver())?;
        self.corrected(&work, &work_part, id, &stored)?;
        self.event("info", &format!("{user} corrected a reading to \"{}\" (correction {cid})", text.trim()));
        Ok(Event::Ok)
    }

    pub fn uncorrect(&self, correction_id: i64) -> Result<Event> {
        let (slot, local) = split(correction_id);
        let part = self.pool.part(slot).ok_or_else(|| anyhow!("that correction no longer exists"))?;
        let Some((file_id, stored, page, x, y, corrected)) = part.with(|s| s.correction_place(local))? else { bail!("that correction no longer exists") };
        let _gate = self.work_gate.lock().unwrap();
        let (mut work, work_part, id) = self.editable(slot, file_id, &stored)?;
        let cid = if part.slot == work_part.slot { local } else { work.find_correction(id, page, x, y, &corrected)?.ok_or_else(|| anyhow!("that correction no longer exists"))? };
        work.uncorrect(cid, &self.deriver())?;
        self.corrected(&work, &work_part, id, &stored)?;
        Ok(Event::Ok)
    }

    pub fn crop_line(&self, line_id: i64, max_px: u32) -> Result<Event> {
        let (slot, local) = split(line_id);
        let part = self.pool.part(slot).ok_or_else(|| anyhow!("that line is no longer in the index"))?;
        let (path, page, b) = part.with(|s| s.line_place(local))?.ok_or_else(|| anyhow!("that line is no longer in the index"))?;
        self.crop(&path, page, b, max_px)
    }

    pub fn history(&self) -> Result<Event> {
        let items = self.with_local(|s| s.history())?.into_iter().map(|(query, at, pinned)| HistoryItem { query, at, pinned }).collect();
        Ok(Event::History { items })
    }

    pub fn pin(&self, query: &str, pinned: bool) -> Result<Event> {
        self.with_local(|s| s.pin(query, pinned, now()))?;
        Ok(Event::Ok)
    }

    pub fn monitor(&self) -> Result<Monitor> {
        let (status, _) = self.status()?;
        // The sums over every part are worked out at most twice a minute.
        let generation = self.pool.generation();
        let memo = self.monitor_memo.lock().unwrap().as_ref().filter(|(g, at, _)| *g == generation && at.elapsed() < Duration::from_secs(30)).map(|(_, _, m)| m.clone());
        let mut m = match memo {
            Some(m) => m,
            None => {
                let m = Box::new(self.monitor_sums());
                *self.monitor_memo.lock().unwrap() = Some((generation, Instant::now(), m.clone()));
                m
            }
        };
        let (p50, p95) = {
            let mut l: Vec<u64> = self.latencies.lock().unwrap().iter().copied().collect();
            l.sort_unstable();
            let at = |q: f64| if l.is_empty() { 0 } else { l[((l.len() - 1) as f64 * q).round() as usize] };
            (at(0.5), at(0.95))
        };
        let mut errors = self.deriver().errors.clone();
        errors.extend(self.pool.refused());
        errors.truncate(20);
        let pcs = self.pcs.lock().unwrap().clone();
        let office_rate: u64 = pcs.iter().filter(|p| now() - p.last_seen < 180).map(|p| p.docs_per_hour).sum::<u64>().max(status.docs_per_hour);
        m.db_bytes = status.store.bytes;
        m.local_bytes = dir_bytes(&self.data);
        m.office_waiting = status.pdfs_waiting;
        m.office_docs_per_hour = office_rate;
        m.office_eta_secs = if office_rate > 0 && status.pdfs_waiting > 0 { status.pdfs_waiting * 3600 / office_rate } else { 0 };
        m.cache_hits = self.cache_hits.load(Ordering::Relaxed);
        m.cache_misses = self.cache_misses.load(Ordering::Relaxed);
        m.cache_claims_waited = self.claims_waited.load(Ordering::Relaxed);
        m.search_p50_ms = p50;
        m.search_p95_ms = p95;
        m.searches = self.searches_total.load(Ordering::Relaxed);
        m.roots = status.index.roots.iter().map(|r| (r.root.clone(), r.state.clone(), r.last_ok, r.note.clone())).collect();
        m.events = self.with_local(|s| s.events(60)).unwrap_or_default();
        m.rule_errors = errors;
        m.backend_note = self.backend_note.lock().unwrap().clone();
        m.change_journal = self.journal_note.lock().unwrap().clone();
        m.pcs = pcs;
        m.status = status;
        Ok(*m)
    }

    /// What the monitor adds up over every part.
    fn monitor_sums(&self) -> Monitor {
        #[derive(Default)]
        struct Sums {
            by_class: Vec<(String, u64)>,
            ocr_pages: u64,
            ocr_ms: u64,
            tag_norms: Vec<String>,
            occurrences: u64,
            with_metadata: u64,
            corrections: u64,
        }
        let per_part = self.pool.each(|_, s| {
            let one = |sql: &str| -> u64 { s.conn.query_row(sql, [], |r| r.get::<_, Option<i64>>(0)).ok().flatten().unwrap_or(0) as u64 };
            let by_class: Vec<(String, u64)> = {
                let mut st = s.conn.prepare("SELECT p.class, COUNT(*) FROM pages p JOIN files f ON f.id=p.file_id WHERE f.read_state=2 GROUP BY p.class")?;
                st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<_>>()?
            };
            let tag_norms: Vec<String> = {
                let mut st = s.conn.prepare("SELECT DISTINCT t.norm FROM tag_occurrences o JOIN engineering_tags t ON t.id=o.tag_id JOIN files f ON f.id=o.file_id")?;
                st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            Ok(Sums {
                by_class,
                ocr_pages: one("SELECT COUNT(*) FROM pages p JOIN files f ON f.id=p.file_id WHERE p.ocr_tiles>p.ocr_skipped"),
                ocr_ms: one("SELECT SUM(p.ocr_ms) FROM pages p JOIN files f ON f.id=p.file_id WHERE p.ocr_tiles>p.ocr_skipped"),
                tag_norms,
                occurrences: one("SELECT COUNT(*) FROM tag_occurrences o JOIN files f ON f.id=o.file_id"),
                with_metadata: one("SELECT COUNT(*) FROM drawing_metadata m JOIN files f ON f.id=m.file_id WHERE m.drawing_no IS NOT NULL"),
                corrections: one("SELECT COUNT(*) FROM corrections c JOIN files f ON f.id=c.file_id WHERE c.stale=0"),
            })
        });
        let mut m = Monitor::default();
        let mut classes: HashMap<String, u64> = HashMap::new();
        let mut norms: HashSet<String> = HashSet::new();
        let (mut ocr_pages, mut ocr_ms) = (0u64, 0u64);
        for (_, s) in per_part {
            for (c, n) in s.by_class {
                *classes.entry(c).or_default() += n;
            }
            ocr_pages += s.ocr_pages;
            ocr_ms += s.ocr_ms;
            norms.extend(s.tag_norms);
            m.tag_occurrences += s.occurrences;
            m.drawings_with_metadata += s.with_metadata;
            m.corrections += s.corrections;
        }
        m.pages_total = classes.values().sum();
        let mut by_class: Vec<(String, u64)> = classes.into_iter().collect();
        by_class.sort_by(|a, b| b.1.cmp(&a.1));
        m.pages_by_class = by_class;
        m.ocr_millis_per_page = ocr_ms.checked_div(ocr_pages).unwrap_or(0);
        m.tags = norms.len() as u64;
        m
    }

    /// What waits (or failed), for the office server's job list. The id is
    /// only good for [`Engine::cancel_job`].
    pub fn jobs(&self, failed: bool, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let id = |path: &str| (crate::pool::path_key(path) & i64::MAX as u64) as i64;
        if failed {
            return Ok(self.counts.lock().unwrap().fails.iter().take(limit).map(|f| (id(&f.path), f.path.clone(), f.reason.clone())).collect());
        }
        let q = self.queue.lock().unwrap();
        let retry = self.retry.lock().unwrap();
        Ok(q.ocr
            .iter()
            .chain(q.fast.iter())
            .take(limit)
            .map(|i| (id(&i.path), i.path.clone(), retry.get(&crate::pool::path_key(&i.path)).map(|r| r.reason.clone()).unwrap_or_default()))
            .collect())
    }

    /// Take a waiting file out of this engine's queue (office server admin).
    pub fn cancel_job(&self, job_id: i64) -> Result<bool> {
        let mut q = self.queue.lock().unwrap();
        let hit = q.ocr.iter().chain(q.fast.iter()).find(|i| (crate::pool::path_key(&i.path) & i64::MAX as u64) as i64 == job_id).map(|i| i.path.clone());
        let Some(path) = hit else { return Ok(false) };
        q.fast.retain(|i| i.path != path);
        q.ocr.retain(|i| i.path != path);
        drop(q);
        self.retry.lock().unwrap().insert(crate::pool::path_key(&path), crate::service::Retry { until: i64::MAX, attempts: 0, reason: "cancelled by an administrator".into() });
        Ok(true)
    }

    /// The path of a read document by the id search results carry.
    pub fn file_path(&self, doc_id: i64) -> Result<Option<String>> {
        let (slot, id) = split(doc_id);
        let Some(part) = self.pool.part(slot) else { return Ok(None) };
        Ok(part.with(|s| s.file_by_id(id))?.map(|f| f.path))
    }

    pub fn file_id(&self, path: &str) -> Result<Option<i64>> {
        Ok(self.pool.covered(&self.stored_path(path)).filter(|c| c.state == state::READ).map(|c| compose(c.slot, c.id)))
    }

    /// Whether the first listing or the reading queue is still unfinished.
    pub fn syncing(&self) -> bool {
        !self.reading_roots_listed() || self.counts.lock().unwrap().waiting > 0
    }
}

/// Size of everything under a folder (this PC's Sleuth data).
fn dir_bytes(dir: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => {
                // Models are the program's, not the index's.
                if e.file_name().eq_ignore_ascii_case("models") { 0 } else { dir_bytes(&e.path()) }
            }
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}
