//! Turning index matches into one result row per file, ranked and explained.
//!
//! Files whose name is exactly what was typed come first, then files whose
//! name (or path) holds every word as typed or with other separators, then
//! the rest (as Everything shows names before contents). Then, best first:
//! 1. exact text as typed, standing on its own
//! 2. the same letters and digits with other separators
//! 3. look-alike characters or a low-confidence reading (verify)
//! 4. only inside a longer tag or word
//!
//! Within a class, text inside the file comes before the file name, the PDF
//! text layer and checked corrections before OCR, higher confidence before
//! lower, and newer files before older. Every row says why it matched.

use crate::store::{Candidate, Scope, Store};
use anyhow::Result;
use sleuth_core::analyzer::{MatchClass, SearchQuery, Term};
use sleuth_core::paths::DriveMap;
use sleuth_core::protocol::{Hit, TextSource};
use std::collections::{HashMap, HashSet};

/// Rows fetched per term before grouping; bounded so a very common word
/// cannot make a search slow.
const PER_TERM: usize = 2000;

/// Search the index. `roots` limits results to paths under those folders
/// (UNC or drive letters). Returns the ranked rows (at most `cap`).
pub fn search_index(store: &Store, q: &SearchQuery, roots: &[String], cap: usize, drives: &DriveMap) -> Result<Vec<Hit>> {
    let mut scope = Scope { roots: roots.iter().map(|r| drives.to_unc(r)).collect(), ..Default::default() };
    scope.ext = q.filters.ext.clone();
    scope.rev = q.filters.rev.clone();
    scope.dwg = q.filters.dwg.clone();
    scope.disc = q.filters.disc.clone();
    let mut best: HashMap<i64, (Candidate, u32)> = HashMap::new();
    // A pasted `T:\folder\file.pdf` is stored under the share's UNC path.
    let groups: Vec<Vec<Term>> = q.groups.iter().filter(|g| !g.is_empty()).map(|g| g.iter().map(|t| to_unc_term(t, drives)).collect()).collect();
    if groups.is_empty() {
        // Only filters (`rev:B dwg:ISO-1001`): list the files they select.
        if q.filters.is_empty() {
            return Ok(Vec::new());
        }
        for id in store.files_in_scope(&scope, cap)? {
            let Some(f) = store.file_by_id(id)? else { continue };
            best.insert(id, (Candidate { file_id: id, page: None, text: f.name.clone(), conf: 1.0, source: TextSource::Metadata, bbox: None, class: MatchClass::Exact, why: describe_filters(q) }, 1));
        }
    }
    for g in &groups {
        let mut found = search_group(store, g, &scope, q.filters.tag_type.as_deref())?;
        // "FV 2201" is one tag typed with a space. When the words find
        // nothing together, search the whole group as one term.
        if found.is_empty() && g.len() > 1 {
            let joined: String = g.iter().map(|t| t.raw.as_str()).collect::<Vec<_>>().join(" ");
            if let Some(whole) = Term::new(&joined) {
                found = search_group(store, std::slice::from_ref(&whole), &scope, q.filters.tag_type.as_deref())?;
            }
        }
        for (id, (c, n)) in found {
            match best.get_mut(&id) {
                Some((b, bn)) if better(&c, b) => {
                    *b = c;
                    *bn = n;
                }
                Some(_) => {}
                None => {
                    best.insert(id, (c, n));
                }
            }
        }
    }
    let mut hits = Vec::new();
    // Drawing details for every PDF at once: a query per row is a disk read
    // per row, which on a hard disk made common words take a minute.
    let pdf_ids: Vec<i64> = best.iter().filter(|(_, (c, _))| c.source != TextSource::FileName || c.text.to_lowercase().ends_with(".pdf")).map(|(id, _)| *id).collect();
    let mut metas = store.metadata_many(&pdf_ids)?;
    for (id, (c, n)) in best {
        let Some(f) = store.file_quick(id)? else { continue };
        if !q.filters.name.is_empty() && !q.filters.name.iter().all(|t| t.classify(&f.name, 1.0).is_some()) {
            continue;
        }
        // `type:` with no terms: files having any tag of that category.
        if let Some(cat) = &q.filters.tag_type
            && groups.is_empty()
            && !store.file_has_tag(id, cat, "")?
        {
            continue;
        }
        let meta = metas.remove(&id).unwrap_or_default();
        hits.push(Hit {
            display_path: drives.to_display(&f.path),
            path: f.path,
            name: f.name,
            page: c.page,
            found: c.text,
            class: c.class,
            source: c.source,
            confidence: c.conf,
            hits: n,
            size: f.size,
            modified: f.mtime,
            why: c.why,
            origin: Default::default(),
            doc_id: Some(id),
            drawing_no: meta.drawing_no,
            revision: meta.revision,
        });
    }
    let tiers: HashMap<i64, u8> = hits.iter().filter_map(|h| h.doc_id.map(|id| (id, name_tier(&groups, &h.name, &h.path)))).collect();
    let tier = |h: &Hit| h.doc_id.and_then(|id| tiers.get(&id).copied()).unwrap_or(2);
    hits.sort_by(|a, b| tier(a).cmp(&tier(b)).then(rank(a).cmp(&rank(b))).then(b.confidence.total_cmp(&a.confidence)).then(b.modified.cmp(&a.modified)).then(a.path.cmp(&b.path)));
    hits.truncate(cap);
    Ok(hits)
}

/// A term written with a mapped drive letter, as the UNC path the index
/// stores.
fn to_unc_term(t: &Term, drives: &DriveMap) -> Term {
    if !t.is_path() || t.raw.starts_with("\\\\") {
        return t.clone();
    }
    let unc = drives.to_unc(&t.raw);
    if unc == t.raw { t.clone() } else { Term::new(&unc).unwrap_or_else(|| t.clone()) }
}

/// How a file's name answers the query, before any other ranking: 0 the
/// name (or path) is exactly what was typed, 1 every word of a group is in
/// the name or path as typed or with other separators, 2 otherwise. So a
/// file found by its name is never buried under text matches inside
/// thousands of drawings, as in Everything.
fn name_tier(groups: &[Vec<Term>], name: &str, path: &str) -> u8 {
    let mut tier = 2;
    for g in groups {
        if g.len() == 1 && g[0].is_whole_name(name, path) {
            return 0;
        }
        if g.iter().all(|t| t.explain_name(name, path).is_some_and(|d| d.class <= MatchClass::SeparatorsIgnored)) {
            tier = 1;
        }
    }
    tier
}

/// Files matching every term of one group: best candidate and matching
/// line count per file. A file matches a term when the term is in its name or
/// in any of its lines. With `category` (`type:` filter) only tags of that
/// category count.
fn search_group(store: &Store, terms: &[Term], scope: &Scope, category: Option<&str>) -> Result<HashMap<i64, (Candidate, u32)>> {
    let mut sets: Vec<HashMap<i64, (Candidate, u32)>> = Vec::new();
    for t in terms {
        let mut m: HashMap<i64, (Candidate, u32)> = HashMap::new();
        let found = match category {
            // `_` alone is never part of a tag.
            Some(_) if t.is_punctuation() => Vec::new(),
            Some(cat) => store.search_tags(t, cat, scope, PER_TERM)?,
            // A path is never inside a drawing's text; punctuation alone is
            // searched in names only (as Everything does).
            None if t.is_path() || t.is_punctuation() => store.search_names(t, scope, PER_TERM)?,
            None => store.search_lines(t, scope, PER_TERM)?.into_iter().chain(store.search_names(t, scope, PER_TERM)?).collect(),
        };
        for c in found {
            match m.get_mut(&c.file_id) {
                Some((b, n)) => {
                    *n += 1;
                    if better(&c, b) {
                        *b = c;
                    }
                }
                None => {
                    m.insert(c.file_id, (c, 1));
                }
            }
        }
        sets.push(m);
    }
    let Some(first) = sets.first() else { return Ok(HashMap::new()) };
    let ids: HashSet<i64> = first.keys().copied().filter(|id| sets[1..].iter().all(|s| s.contains_key(id))).collect();
    let mut out = HashMap::new();
    for id in ids {
        // The row shows the first term's best match; its class is the
        // weakest of all terms, so two exact terms and one look-alike is a
        // look-alike result.
        let (mut c, n) = sets[0][&id].clone();
        let worst = sets.iter().map(|s| &s[&id].0).max_by_key(|x| x.class).unwrap();
        if worst.class > c.class {
            c.why = format!("{}; {}", c.why, worst.why.to_lowercase());
            c.class = worst.class;
        }
        out.insert(id, (c, n));
    }
    // Several words: also every word in the file name or folder path, found
    // from the most specific word so no per-word limit drops the file.
    if category.is_none() && terms.len() > 1 {
        for c in store.search_names_all(terms, scope, PER_TERM)? {
            match out.get_mut(&c.file_id) {
                Some((b, _)) if better(&c, b) => *b = c,
                Some(_) => {}
                None => {
                    out.insert(c.file_id, (c, 1));
                }
            }
        }
    }
    Ok(out)
}

fn source_rank(s: TextSource) -> u8 {
    match s {
        TextSource::TextLayer | TextSource::Verified => 0,
        TextSource::Ocr => 1,
        TextSource::Metadata => 2,
        TextSource::FileName | TextSource::Pending => 3,
    }
}

fn rank(h: &Hit) -> (MatchClass, u8) {
    (h.class, source_rank(h.source))
}

/// Prefer stricter matches, then text inside the file, then confidence.
fn better(c: &Candidate, best: &Candidate) -> bool {
    (c.class, source_rank(c.source)) < (best.class, source_rank(best.source)) || ((c.class, source_rank(c.source)) == (best.class, source_rank(best.source)) && c.conf > best.conf)
}

fn describe_filters(q: &SearchQuery) -> String {
    let f = &q.filters;
    let mut parts = Vec::new();
    if let Some(v) = &f.dwg {
        parts.push(format!("drawing number contains {v}"));
    }
    if let Some(v) = &f.rev {
        parts.push(format!("revision {v}"));
    }
    if let Some(v) = &f.disc {
        parts.push(format!("discipline {v}"));
    }
    if let Some(v) = &f.tag_type {
        parts.push(format!("has {v} tags"));
    }
    if let Some(v) = &f.ext {
        parts.push(format!(".{v} files"));
    }
    if parts.is_empty() { "Matches the filters".into() } else { format!("Title block: {}", parts.join(", ")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::Deriver;
    use crate::store::state;
    use crate::text::{FileText, Line, PageText};
    use sleuth_core::analyzer::parse_search;
    use sleuth_core::protocol::NormBox;

    fn store_with(files: &[(&str, &str, f32)]) -> Store {
        let mut s = Store::open_in_memory().unwrap();
        let d = Deriver::new(&sleuth_core::settings::Settings::default());
        for (i, (path, text, conf)) in files.iter().enumerate() {
            let (dir, name) = path.rsplit_once('\\').unwrap();
            let (id, _) = s.upsert_file(path, dir, name, 1, i as i64, 1, state::WAITING).unwrap();
            let src = if *conf < 1.0 { TextSource::Ocr } else { TextSource::TextLayer };
            let line = Line { text: (*text).into(), conf: *conf, source: src, bbox: NormBox { x: 0.1, y: 0.1, w: 0.1, h: 0.01 }, alt: false };
            s.save_read(id, &format!("h{i}"), &FileText { reader_version: 2, model: "m".into(), pages: vec![PageText { page: 1, lines: vec![line] }], ..Default::default() }, 1, &d).unwrap();
        }
        s
    }

    #[test]
    fn tag_typed_with_a_space_is_found() {
        let s = store_with(&[(r"\\fs01\p\pid.pdf", "FV-2201", 0.99)]);
        let hits = search_index(&s, &parse_search("FV 2201"), &[], 10, &DriveMap::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].class, MatchClass::SeparatorsIgnored);
        assert!(!hits[0].why.is_empty());
    }

    #[test]
    fn or_and_ranking() {
        let s = store_with(&[
            (r"C:\p\a.pdf", "PSV-1203", 1.0),
            (r"C:\p\b.pdf", "PSV-12O3", 0.95),
            (r"C:\p\c.pdf", "PSV-12034", 1.0),
            (r"C:\p\d.pdf", "PSV-1204", 1.0),
        ]);
        let hits = search_index(&s, &parse_search("PSV-1203"), &[], 10, &DriveMap::default()).unwrap();
        let order: Vec<(&str, MatchClass)> = hits.iter().map(|h| (h.name.as_str(), h.class)).collect();
        assert_eq!(order, vec![("a.pdf", MatchClass::Exact), ("b.pdf", MatchClass::Uncertain), ("c.pdf", MatchClass::Partial)]);
        let both = search_index(&s, &parse_search("PSV-1203 OR PSV-1204"), &[], 10, &DriveMap::default()).unwrap();
        assert_eq!(both.iter().filter(|h| h.class == MatchClass::Exact).count(), 2);
    }

    /// Names only (nothing read yet), as on the user's PC.
    fn names_only(paths: &[&str]) -> Store {
        let s = Store::open_in_memory().unwrap();
        for p in paths {
            let (dir, name) = p.rsplit_once('\\').unwrap();
            s.touch_dir(dir, None, 1).unwrap();
            let st = if name.to_lowercase().ends_with(".pdf") { state::UNREAD } else { state::NOT_PDF };
            s.upsert_file(p, dir, name, 1, 1, 1, st).unwrap();
        }
        s
    }

    const SHARE: &str = r"\\10.0.0.54\docs-latest";

    fn t_drive() -> DriveMap {
        use sleuth_core::paths::{Drive, DriveKind};
        DriveMap { drives: vec![Drive { letter: "T:".into(), kind: DriveKind::Network, unc: Some(SHARE.into()) }] }
    }

    fn names_of(s: &Store, q: &str) -> Vec<String> {
        search_index(s, &parse_search(q), &[], 100, &t_drive()).unwrap().into_iter().map(|h| h.name).collect()
    }

    #[test]
    fn user_name_queries_find_every_file_type() {
        let iso = format!(r"{SHARE}\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf");
        let iso2 = format!(r"{SHARE}\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\ISOMETRIC ISSUANCE_26-FEB-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf");
        let s = names_only(&[
            r"D:\lots\FSK_Batch-1.zip",
            r"D:\lots\FSK_Batch-12.zip",
            r"D:\FSK As-Built request_18-Sep-2026.xlsx",
            r"D:\cad\FS-101.dwg",
            r"\\10.0.0.20\Project\P20002-14-01-63-1636\MAIN DOC\P20002-14-01-63-1636-REV.2.xlsx",
            r"\\10.0.0.20\Project\x\ISO-1636.pdf",
            iso.as_str(),
            iso2.as_str(),
            r"C:\x\notes.txt",
        ]);
        assert_eq!(names_of(&s, "FSK_Batch-1")[0], "FSK_Batch-1.zip");
        assert_eq!(names_of(&s, "FSK").len(), 3);
        assert_eq!(names_of(&s, "FS").len(), 4);
        assert_eq!(names_of(&s, "1636").len(), 2);
        assert_eq!(names_of(&s, "*1636")[0], "ISO-1636.pdf");
        assert_eq!(names_of(&s, "PSV").len(), 2);
        assert_eq!(names_of(&s, "zip").len(), 2);
        // a path pasted with its mapped drive letter and spaces
        let hits = search_index(&s, &parse_search(r"T:\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf"), &[], 10, &t_drive()).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].path, iso);
        assert_eq!(hits[0].display_path, r"T:\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf");
        // quoted, too; and a piece of a path
        assert_eq!(names_of(&s, r#""T:\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf""#).len(), 1);
        assert_eq!(names_of(&s, r"2632\14-01-15-2632_PSV").len(), 2);
        // several words: AND across name and folder path
        assert_eq!(names_of(&s, "ISSUANCE PSV-2809 hold").len(), 1);
        assert_eq!(names_of(&s, "FSK zip").len(), 2);
        assert!(names_of(&s, "FSK dwg").is_empty());
    }

    #[test]
    fn exact_name_ranks_before_text_inside_drawings() {
        let mut files: Vec<(String, &str, f32)> = (0..30).map(|i| (format!(r"\\fs\p\iso{i}.pdf"), "NOTE: SEE PSV-2809 FOR SETTINGS", 1.0)).collect();
        files.push((r"\\fs\p\PSV-2809.pdf".into(), "GENERAL ARRANGEMENT", 1.0));
        let refs: Vec<(&str, &str, f32)> = files.iter().map(|(p, t, c)| (p.as_str(), *t, *c)).collect();
        let s = store_with(&refs);
        let hits = search_index(&s, &parse_search("PSV-2809"), &[], 100, &DriveMap::default()).unwrap();
        assert_eq!(hits.len(), 31);
        assert_eq!(hits[0].name, "PSV-2809.pdf");
        assert_eq!(hits[0].source, TextSource::FileName);
    }

    #[test]
    fn filters_only_search_uses_title_block() {
        let s = store_with(&[(r"C:\p\x.pdf", "DRAWING No.: ISO-1001-01", 1.0), (r"C:\p\y.pdf", "DRAWING No.: PID-0101", 1.0)]);
        let hits = search_index(&s, &parse_search("dwg:ISO-1001"), &[], 10, &DriveMap::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].drawing_no.as_deref(), Some("ISO-1001-01"));
        assert_eq!(search_index(&s, &parse_search("type:drawing PID"), &[], 10, &DriveMap::default()).unwrap().len(), 1);
        // a two-letter term works with type:
        let s = store_with(&[(r"C:\p\a.pdf", "FV-2201 GLOBE", 1.0), (r"C:\p\b.pdf", "FVR NOTE 1", 1.0)]);
        let hits = search_index(&s, &parse_search("type:instrument FV"), &[], 10, &DriveMap::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "a.pdf", "FVR is not an instrument tag");
    }
}
