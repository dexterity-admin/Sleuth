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
    let hits = search_terms(store, terms, roots, limit, drives)?;
    // "FV 2201" is one tag typed with a space. When the words find nothing
    // apart, search the whole query as one term with the spaces ignored.
    if hits.is_empty() && terms.len() > 1 {
        let joined: String = terms.iter().map(|t| t.raw.as_str()).collect::<Vec<_>>().join(" ");
        if let Some(whole) = Term::new(&joined) {
            return search_terms(store, std::slice::from_ref(&whole), roots, limit, drives);
        }
    }
    Ok(hits)
}

fn search_terms(store: &Store, terms: &[Term], roots: &[String], limit: usize, drives: &DriveMap) -> Result<Vec<Hit>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{FileText, Line, PageText};
    use sleuth_core::analyzer::{MatchClass, parse_query};
    use sleuth_core::protocol::NormBox;

    #[test]
    fn tag_typed_with_a_space_is_found() {
        let mut s = Store::open_in_memory().unwrap();
        let (id, _) = s.upsert_file(r"\fs01\p\pid.pdf", r"\fs01\p", "pid.pdf", 1, 1, 1, Some(true)).unwrap();
        let line = Line { text: "FV-2201".into(), conf: 0.99, source: TextSource::Ocr, bbox: NormBox { x: 0.1, y: 0.1, w: 0.1, h: 0.01 }, alt: false };
        s.save_read(id, "h", &FileText { reader_version: 1, model: "m".into(), pages: vec![PageText { page: 1, lines: vec![line] }] }, 1).unwrap();
        let hits = search_index(&s, &parse_query("FV 2201"), &[], 10, &DriveMap::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].class, MatchClass::SeparatorsIgnored);
    }
}
