//! Turning index matches into one result row per file.

use crate::store::{Candidate, Store};
use anyhow::Result;
use sleuth_core::analyzer::Term;
use sleuth_core::paths::DriveMap;
use sleuth_core::protocol::{Hit, TextSource};
use std::collections::{HashMap, HashSet};

/// Search the index for files matching every term (in their name or in any
/// line of their text). `roots` limits results to paths under those folders.
pub fn search_index(store: &Store, terms: &[Term], roots: &[String], limit: usize, drives: &DriveMap) -> Result<Vec<Hit>> {
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let per_term = limit.max(200) * 4;
    // Best candidate and hit count per file, for each term.
    let mut sets: Vec<HashMap<i64, (Candidate, u32)>> = Vec::new();
    for t in terms {
        let mut m: HashMap<i64, (Candidate, u32)> = HashMap::new();
        for c in store.search_lines(t, per_term)?.into_iter().chain(store.search_names(t, per_term)?) {
            match m.get_mut(&c.file_id) {
                Some((best, n)) => {
                    *n += 1;
                    if better(&c, best) {
                        *best = c;
                    }
                }
                None => {
                    m.insert(c.file_id, (c, 1));
                }
            }
        }
        sets.push(m);
    }
    let mut ids: HashSet<i64> = sets[0].keys().copied().collect();
    for s in &sets[1..] {
        ids.retain(|id| s.contains_key(id));
    }
    let roots: Vec<String> = roots.iter().map(|r| drives.to_unc(r).to_lowercase()).collect();
    let mut hits = Vec::new();
    for id in ids {
        let Some(f) = store.file_by_id(id)? else { continue };
        if !roots.is_empty() && !roots.iter().any(|r| f.path.to_lowercase().starts_with(r.trim_end_matches('\\'))) {
            continue;
        }
        let (best, n) = &sets[0][&id];
        let worst = sets.iter().map(|s| s[&id].0.class).max().unwrap_or(best.class);
        hits.push(Hit {
            display_path: drives.to_display(&f.path),
            path: f.path,
            name: f.name,
            page: best.page,
            found: best.text.clone(),
            class: worst,
            source: best.source,
            confidence: best.conf,
            hits: *n,
            size: f.size,
            modified: f.mtime,
        });
    }
    hits.sort_by(|a, b| a.class.cmp(&b.class).then(content_first(a).cmp(&content_first(b))).then(b.modified.cmp(&a.modified)));
    hits.truncate(limit);
    Ok(hits)
}

fn content_first(h: &Hit) -> u8 {
    if h.source == TextSource::FileName { 1 } else { 0 }
}

/// Prefer stricter matches, then text found inside the file over its name.
fn better(c: &Candidate, best: &Candidate) -> bool {
    (c.class, c.source == TextSource::FileName) < (best.class, best.source == TextSource::FileName)
}
