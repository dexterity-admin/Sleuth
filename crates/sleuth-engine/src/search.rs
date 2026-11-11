//! Turning matches into one result row per file, ranked and explained.
//!
//! File names come from the in-memory catalog (every file on every drive);
//! text comes from the parts pool (every document read, by any PC). Both
//! are keyed by path, so a file found by one word in its name and another
//! word inside it is one row.
//!
//! Files whose name is exactly what was typed come first, then files whose
//! name (or path) holds every word as typed or with other separators, then
//! the rest (as Everything shows names before contents). Then, best first:
//! 1. exact text as typed, standing on its own
//! 2. the same letters and digits with other separators
//! 3. look-alike characters or a low-confidence reading (verify)
//! 4. only inside a longer tag or word
//!
//! Within a class, text inside the file comes before the file name, text
//! taken from the file itself and checked corrections before OCR, higher
//! confidence before lower, and newer files before older. Every row says
//! why it matched.

use crate::pool::{Pool, compose};
use crate::reader::DRAWING_SIDE_PT;
use crate::store::{Candidate, FileRow, Scope, Store, all_terms_candidate, name_candidate, state};
use anyhow::Result;
use sleuth_core::analyzer::{MatchClass, SearchQuery, Term, norm_form};
use sleuth_core::kinds::FileKind;
use sleuth_core::paths::DriveMap;
use sleuth_core::protocol::{DrawingMeta, Hit, TextSource, Twin};
use sleuth_office::{Catalog, FileInfo, NameScope};
use std::collections::{HashMap, HashSet};

/// Rows fetched per term (and per part) before grouping; bounded so a very
/// common word cannot make a search slow.
const PER_TERM: usize = 2000;
/// Other files of the same document listed on one row.
const MAX_TWINS: usize = 8;

/// A file a match belongs to.
#[derive(Debug, Clone)]
struct Doc {
    path: String,
    name: String,
    size: u64,
    mtime: i64,
    /// The part holding its reading and the row id there, once known.
    at: Option<(u32, i64)>,
}

impl Doc {
    fn read(f: FileRow, slot: u32) -> Doc {
        Doc { at: Some((slot, f.id)), path: f.path, name: f.name, size: f.size, mtime: f.mtime }
    }
    fn named(f: FileInfo) -> Doc {
        Doc { path: f.path, name: f.name, size: f.size, mtime: f.mtime, at: None }
    }
}

/// Best candidate and matching-line count per file, by lower-case path.
type Found = HashMap<String, (Candidate, u32, Doc)>;

fn meta_scope(scope: &Scope) -> bool {
    scope.rev.is_some() || scope.dwg.is_some() || scope.disc.is_some()
}

fn name_scope(scope: &Scope) -> NameScope {
    NameScope { roots: scope.roots.clone(), ext: scope.ext.clone() }
}

/// Search names and read documents. `roots` limits results to paths under
/// those folders (network paths or drive letters). Returns the ranked rows
/// (at most `cap`).
pub fn search_index(pool: &Pool, catalog: &Catalog, q: &SearchQuery, roots: &[String], cap: usize, drives: &DriveMap) -> Result<Vec<Hit>> {
    let mut scope = Scope { roots: roots.iter().map(|r| drives.to_unc(r)).collect(), ..Default::default() };
    scope.ext = q.filters.ext.clone();
    scope.rev = q.filters.rev.clone();
    scope.dwg = q.filters.dwg.clone();
    scope.disc = q.filters.disc.clone();
    let mut best: Found = HashMap::new();
    // A pasted `T:\folder\file.pdf` is stored under the share's network path.
    let groups: Vec<Vec<Term>> = q.groups.iter().filter(|g| !g.is_empty()).map(|g| g.iter().map(|t| to_unc_term(t, drives)).collect()).collect();
    if groups.is_empty() {
        // Only filters (`rev:B dwg:ISO-1001`): list the files they select.
        if q.filters.is_empty() {
            return Ok(Vec::new());
        }
        best = filters_only(pool, catalog, q, &scope, cap);
    }
    for g in &groups {
        let mut found = search_group(pool, catalog, g, &scope, q.filters.tag_type.as_deref());
        // "FV 2201" is one tag typed with a space. When the words find
        // nothing together, search the whole group as one term.
        if found.is_empty() && g.len() > 1 {
            let joined: String = g.iter().map(|t| t.raw.as_str()).collect::<Vec<_>>().join(" ");
            if let Some(whole) = Term::new(&joined) {
                found = search_group(pool, catalog, std::slice::from_ref(&whole), &scope, q.filters.tag_type.as_deref());
            }
        }
        for (key, (c, n, doc)) in found {
            match best.get_mut(&key) {
                Some((b, bn, bd)) => {
                    if bd.at.is_none() {
                        bd.at = doc.at;
                    }
                    if better(&c, b) {
                        *b = c;
                        *bn = n;
                    }
                }
                None => {
                    best.insert(key, (c, n, doc));
                }
            }
        }
    }
    // A file found by its name may have been read too: its row then shows
    // its drawing number and opens with its text marked.
    for (_, _, doc) in best.values_mut() {
        if doc.at.is_none()
            && let Some(c) = pool.covered(&doc.path)
            && c.state == state::READ
        {
            doc.at = Some((c.slot, c.id));
        }
    }
    // Title-block details and page sizes, one query per part.
    let mut by_slot: HashMap<u32, Vec<i64>> = HashMap::new();
    for (_, _, doc) in best.values() {
        if let Some((slot, id)) = doc.at {
            by_slot.entry(slot).or_default().push(id);
        }
    }
    let mut metas: HashMap<(u32, i64), DrawingMeta> = HashMap::new();
    let mut sheets: HashSet<(u32, i64)> = HashSet::new();
    let mut typed: HashSet<(u32, i64)> = HashSet::new();
    let type_only = q.filters.tag_type.as_deref().filter(|_| groups.is_empty());
    for (slot, ids) in by_slot {
        let Some(part) = pool.part(slot) else { continue };
        let r = part.with(|s| {
            let mut tagged = Vec::new();
            if let Some(cat) = type_only {
                for id in &ids {
                    if s.file_has_tag(*id, cat, "")? {
                        tagged.push(*id);
                    }
                }
            }
            Ok((s.metadata_many(&ids)?, s.drawing_sized(&ids, DRAWING_SIDE_PT)?, tagged))
        });
        if let Ok((m, big, tagged)) = r {
            metas.extend(m.into_iter().map(|(id, m)| ((slot, id), m)));
            sheets.extend(big.into_iter().map(|id| (slot, id)));
            typed.extend(tagged.into_iter().map(|id| (slot, id)));
        }
    }
    let mut hits: Vec<(u8, Hit)> = Vec::new();
    for (_, (c, n, doc)) in best {
        if !q.filters.name.is_empty() && !q.filters.name.iter().all(|t| t.classify(&doc.name, 1.0).is_some()) {
            continue;
        }
        // `type:` with no terms: files having any tag of that category.
        if type_only.is_some() && !doc.at.is_some_and(|at| typed.contains(&at)) {
            continue;
        }
        let meta = doc.at.and_then(|at| metas.remove(&at)).unwrap_or_default();
        let kind = FileKind::of(&doc.name);
        let is_drawing = meta.drawing_no.is_some() || doc.at.is_some_and(|at| sheets.contains(&at));
        let tier = name_tier(&groups, &doc.name, &doc.path);
        hits.push((
            tier,
            Hit {
                display_path: drives.to_display(&doc.path),
                path: doc.path,
                name: doc.name,
                page: c.page,
                found: c.text,
                class: c.class,
                source: c.source,
                confidence: c.conf,
                hits: n,
                size: doc.size,
                modified: doc.mtime,
                why: c.why,
                origin: Default::default(),
                doc_id: doc.at.map(|(slot, id)| compose(slot, id)),
                drawing_no: meta.drawing_no,
                revision: meta.revision,
                kind,
                category: kind.category(is_drawing),
                twins: Vec::new(),
            },
        ));
    }
    hits.sort_by(|(ta, a), (tb, b)| ta.cmp(tb).then(rank(a).cmp(&rank(b))).then(b.confidence.total_cmp(&a.confidence)).then(b.modified.cmp(&a.modified)).then(a.path.cmp(&b.path)));
    hits.truncate(cap);
    Ok(hits.into_iter().map(|(_, h)| h).collect())
}

/// Fill in, for each row, the other files of the same document: the same
/// name (without its extension) in the same folder or a folder beside it,
/// or the same drawing number and revision in a title block. Only files of
/// another type count: two PDFs are two documents. Nothing is guessed: the
/// names or the title-block details must be equal.
pub fn attach_twins(hits: &mut [Hit], pool: &Pool, catalog: &Catalog, drives: &DriveMap) {
    // Title blocks: every read document with one of these drawing numbers.
    let norms: Vec<String> = hits.iter().filter_map(|h| h.drawing_no.as_deref().map(norm_form)).filter(|n| !n.is_empty()).collect::<HashSet<_>>().into_iter().collect();
    let mut by_drawing: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    if !norms.is_empty() {
        for (_, rows) in pool.each(|_, s| s.docs_by_drawing(&norms)) {
            for (dwg, rev, path, name) in rows {
                by_drawing.entry((dwg, rev.unwrap_or_default())).or_default().push((path, name));
            }
        }
    }
    for h in hits.iter_mut() {
        let mut seen: HashSet<String> = HashSet::from([h.path.to_lowercase()]);
        let mut twins = Vec::new();
        let dir = h.path.rsplit_once('\\').map(|(d, _)| d.to_lowercase()).unwrap_or_default();
        for f in catalog.twins(&h.path) {
            let kind = FileKind::of(&f.name);
            if kind == h.kind || !seen.insert(f.path.to_lowercase()) {
                continue;
            }
            let same_dir = f.path.rsplit_once('\\').is_some_and(|(d, _)| d.to_lowercase() == dir);
            let why = if same_dir { "same file name in the same folder" } else { "same file name in a folder beside it" };
            twins.push(Twin { display_path: drives.to_display(&f.path), path: f.path, name: f.name, kind, why: why.into() });
        }
        if let Some(dwg) = h.drawing_no.as_deref().map(norm_form).filter(|n| !n.is_empty()) {
            let rev = h.revision.as_deref().map(norm_form).unwrap_or_default();
            for (path, name) in by_drawing.get(&(dwg, rev)).into_iter().flatten() {
                let kind = FileKind::of(name);
                if kind == h.kind || !seen.insert(path.to_lowercase()) {
                    continue;
                }
                twins.push(Twin { display_path: drives.to_display(path), path: path.clone(), name: name.clone(), kind, why: "same drawing number and revision in the title block".into() });
            }
        }
        twins.truncate(MAX_TWINS);
        h.twins = twins;
    }
}

/// A term written with a mapped drive letter, as the network path stored.
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

/// Candidates of one part with the files they belong to (hidden readings
/// have no file and drop out).
fn resolve(s: &Store, cands: Vec<Candidate>) -> Result<Vec<(Candidate, FileRow)>> {
    let ids: Vec<i64> = cands.iter().map(|c| c.file_id).collect::<HashSet<_>>().into_iter().collect();
    let files = s.files_by_ids(&ids)?;
    Ok(cands.into_iter().filter_map(|c| files.get(&c.file_id).cloned().map(|f| (c, f))).collect())
}

/// Everything matching one term: lines (or tags of `category`) in read
/// documents, and file names.
fn term_matches(pool: &Pool, catalog: &Catalog, t: &Term, scope: &Scope, category: Option<&str>) -> Vec<(Candidate, Doc)> {
    let mut out: Vec<(Candidate, Doc)> = Vec::new();
    let in_text = match category {
        // `_` alone is never part of a tag.
        Some(_) => !t.is_punctuation(),
        // A path is never inside a document's text; punctuation alone is
        // searched in names only (as Everything does).
        None => !(t.is_path() || t.is_punctuation()),
    };
    if in_text {
        let found = pool.each(|_, s| {
            let cands = match category {
                Some(cat) => s.search_tags(t, cat, scope, PER_TERM)?,
                None => s.search_lines(t, scope, PER_TERM)?,
            };
            resolve(s, cands)
        });
        for (part, rows) in found {
            out.extend(rows.into_iter().map(|(c, f)| (c, Doc::read(f, part.slot))));
        }
    }
    if category.is_none() {
        out.extend(name_matches(pool, catalog, t, scope));
    }
    out
}

/// Files whose name (or path, for a term with `\`) matches one term.
fn name_matches(pool: &Pool, catalog: &Catalog, t: &Term, scope: &Scope) -> Vec<(Candidate, Doc)> {
    if meta_scope(scope) {
        // Title-block filters only know read documents.
        let found = pool.each(|_, s| resolve(s, s.search_names(t, scope, PER_TERM)?));
        return found.into_iter().flat_map(|(part, rows)| rows.into_iter().map(move |(c, f)| (c, Doc::read(f, part.slot)))).collect();
    }
    let ns = name_scope(scope);
    let rows = if t.is_path() {
        // Up to the first wildcard; the term itself decides after that.
        catalog.candidates_path(t.raw.split(['*', '?']).next().unwrap_or(""), &ns, PER_TERM * 4)
    } else {
        catalog.candidates(t, &ns, PER_TERM * 4)
    };
    let mut found: Vec<(bool, Candidate, Doc)> = rows.into_iter().filter_map(|f| name_candidate(t, 0, &f.name, &f.path).map(|(whole, c)| (whole, c, Doc::named(f)))).collect();
    found.sort_by_key(|(whole, c, _)| (c.class, !*whole));
    found.truncate(PER_TERM);
    found.into_iter().map(|(_, c, d)| (c, d)).collect()
}

/// Files where every term is in the name or the folder path.
fn names_all(pool: &Pool, catalog: &Catalog, terms: &[Term], scope: &Scope) -> Vec<(Candidate, Doc)> {
    if meta_scope(scope) {
        let found = pool.each(|_, s| resolve(s, s.search_names_all(terms, scope, PER_TERM)?));
        return found.into_iter().flat_map(|(part, rows)| rows.into_iter().map(move |(c, f)| (c, Doc::read(f, part.slot)))).collect();
    }
    let ns = name_scope(scope);
    let rows = match terms.iter().find(|t| t.is_path()) {
        // A piece of a path among the words: start from the files it names.
        Some(p) => catalog.candidates_path(p.raw.split(['*', '?']).next().unwrap_or(""), &ns, PER_TERM * 4),
        None => catalog.candidates_all(terms, &ns, PER_TERM * 4),
    };
    let mut found: Vec<(Candidate, Doc)> = rows.into_iter().filter_map(|f| all_terms_candidate(terms, 0, &f.name, &f.path).map(|c| (c, Doc::named(f)))).collect();
    found.sort_by_key(|(c, _)| c.class);
    found.truncate(PER_TERM);
    found
}

/// Files matching every term of one group: best candidate and matching
/// line count per file. A file matches a term when the term is in its name or
/// in any of its lines. With `category` (`type:` filter) only tags of that
/// category count.
fn search_group(pool: &Pool, catalog: &Catalog, terms: &[Term], scope: &Scope, category: Option<&str>) -> Found {
    let mut sets: Vec<Found> = Vec::new();
    for t in terms {
        let mut m: Found = HashMap::new();
        for (c, doc) in term_matches(pool, catalog, t, scope, category) {
            match m.get_mut(&doc.path.to_lowercase()) {
                Some((b, n, d)) => {
                    *n += 1;
                    if d.at.is_none() {
                        d.at = doc.at;
                    }
                    if better(&c, b) {
                        *b = c;
                    }
                }
                None => {
                    m.insert(doc.path.to_lowercase(), (c, 1, doc));
                }
            }
        }
        sets.push(m);
    }
    let Some(first) = sets.first() else { return HashMap::new() };
    let keys: Vec<String> = first.keys().filter(|k| sets[1..].iter().all(|s| s.contains_key(*k))).cloned().collect();
    let mut out: Found = HashMap::new();
    for key in keys {
        // The row shows the first term's best match; its class is the
        // weakest of all terms, so two exact terms and one look-alike is a
        // look-alike result.
        let (mut c, n, mut doc) = sets[0][&key].clone();
        let worst = sets.iter().map(|s| &s[&key].0).max_by_key(|x| x.class).unwrap();
        if worst.class > c.class {
            c.why = format!("{}; {}", c.why, worst.why.to_lowercase());
            c.class = worst.class;
        }
        if doc.at.is_none() {
            doc.at = sets.iter().find_map(|s| s[&key].2.at);
        }
        out.insert(key, (c, n, doc));
    }
    // Several words: also every word in the file name or folder path, found
    // from all the words at once so no per-word limit drops the file.
    if category.is_none() && terms.len() > 1 {
        for (c, doc) in names_all(pool, catalog, terms, scope) {
            match out.get_mut(&doc.path.to_lowercase()) {
                Some((b, _, _)) => {
                    if better(&c, b) {
                        *b = c;
                    }
                }
                None => {
                    out.insert(doc.path.to_lowercase(), (c, 1, doc));
                }
            }
        }
    }
    out
}

/// The files a query of filters alone selects, newest first.
fn filters_only(pool: &Pool, catalog: &Catalog, q: &SearchQuery, scope: &Scope, cap: usize) -> Found {
    let mut out: Found = HashMap::new();
    let why = describe_filters(q);
    let cand = |name: &str| Candidate { file_id: 0, page: None, text: name.to_string(), conf: 1.0, source: TextSource::Metadata, bbox: None, class: MatchClass::Exact, why: why.clone() };
    if meta_scope(scope) || q.filters.tag_type.is_some() {
        // Title-block and tag filters select among read documents.
        let found = pool.each(|_, s| {
            let ids = s.files_in_scope(scope, cap)?;
            Ok(s.files_by_ids(&ids)?.into_values().collect::<Vec<FileRow>>())
        });
        for (part, rows) in found {
            for f in rows {
                out.insert(f.path.to_lowercase(), (cand(&f.name), 1, Doc::read(f, part.slot)));
            }
        }
        return out;
    }
    // `ext:` and `name:` alone select among every file by name.
    let ns = name_scope(scope);
    let rows = if !q.filters.name.is_empty() {
        catalog.candidates_all(&q.filters.name, &ns, cap)
    } else {
        match scope.ext.as_ref().and_then(|e| Term::new(&format!("*.{e}"))) {
            Some(t) => catalog.candidates(&t, &ns, cap),
            None => Vec::new(),
        }
    };
    for f in rows {
        out.insert(f.path.to_lowercase(), (cand(&f.name), 1, Doc::named(f)));
    }
    out
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
pub(crate) mod tests {
    use super::*;
    use crate::derive::Deriver;
    use crate::pool::Presence;
    use crate::text::{FileText, Line, PageText};
    use sleuth_core::analyzer::parse_search;
    use sleuth_core::paths::root_of;
    use sleuth_core::protocol::NormBox;
    use sleuth_office::Listing;
    use std::collections::BTreeMap;

    /// A catalog and a pool as a PC has them, built from a few files.
    pub(crate) struct Office {
        pub pool: Pool,
        pub catalog: Catalog,
    }

    /// Put files in a catalog, folder by folder.
    pub(crate) fn catalog_of(paths: &[&str]) -> Catalog {
        let cat = Catalog::new();
        let mut dirs: BTreeMap<String, Vec<(String, u64, i64)>> = BTreeMap::new();
        for (i, p) in paths.iter().enumerate() {
            cat.add_root(&root_of(p));
            let (dir, name) = p.rsplit_once('\\').unwrap();
            let dir = if dir.len() == 2 { format!("{dir}\\") } else { dir.to_string() };
            dirs.entry(dir).or_default().push((name.to_string(), 1, i as i64 + 1));
        }
        // Parents first, each listing its subfolders.
        let all: Vec<String> = dirs.keys().cloned().collect();
        let mut every: BTreeMap<String, (Vec<String>, Vec<(String, u64, i64)>)> = BTreeMap::new();
        for d in &all {
            let files = dirs[d].clone();
            every.entry(d.clone()).or_default().1 = files;
            let root = root_of(d);
            let mut cur = d.clone();
            while cur.len() > root.trim_end_matches('\\').len() + 1 {
                let Some((parent, name)) = cur.rsplit_once('\\') else { break };
                let parent = if parent.len() == 2 { format!("{parent}\\") } else { parent.to_string() };
                if parent.len() < root.trim_end_matches('\\').len() {
                    break;
                }
                let e = every.entry(parent.clone()).or_default();
                if !e.0.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                    e.0.push(name.to_string());
                }
                if parent == cur {
                    break;
                }
                cur = parent;
            }
        }
        for (dir, (subdirs, files)) in every {
            cat.apply(&Listing { dir, pass: 1, listed_at: 1, subdirs, files });
        }
        cat
    }

    /// Files read with one line of text each (`conf` below 1 means OCR).
    pub(crate) fn office_with(files: &[(&str, &str, f32)]) -> Office {
        let mut s = Store::open_in_memory().unwrap();
        let d = Deriver::new(&sleuth_core::settings::Settings::default());
        for (i, (path, text, conf)) in files.iter().enumerate() {
            let (dir, name) = path.rsplit_once('\\').unwrap();
            let (id, _) = s.upsert_file(path, dir, name, 1, i as i64 + 1, 1, state::WAITING).unwrap();
            let src = if *conf < 1.0 { TextSource::Ocr } else { TextSource::TextLayer };
            let line = Line { text: (*text).into(), conf: *conf, source: src, bbox: NormBox { x: 0.1, y: 0.1, w: 0.1, h: 0.01 }, alt: false };
            s.save_read(id, &format!("h{i}"), &FileText { reader_version: 2, model: "m".into(), pages: vec![PageText { page: 1, lines: vec![line] }], ..Default::default() }, 1, &d).unwrap();
        }
        let paths: Vec<&str> = files.iter().map(|f| f.0).collect();
        let pool = Pool::new();
        pool.add_store(s, "test");
        pool.recover(&|_| Presence::Here);
        Office { pool, catalog: catalog_of(&paths) }
    }

    /// Names only (nothing read yet), as on a new PC.
    fn names_only(paths: &[&str]) -> Office {
        Office { pool: Pool::new(), catalog: catalog_of(paths) }
    }

    fn find(o: &Office, q: &str, drives: &DriveMap) -> Vec<Hit> {
        search_index(&o.pool, &o.catalog, &parse_search(q), &[], 100, drives).unwrap()
    }

    #[test]
    fn tag_typed_with_a_space_is_found() {
        let o = office_with(&[(r"\\fs01\p\pid.pdf", "FV-2201", 0.99)]);
        let hits = find(&o, "FV 2201", &DriveMap::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].class, MatchClass::SeparatorsIgnored);
        assert!(!hits[0].why.is_empty());
    }

    #[test]
    fn or_and_ranking() {
        let o = office_with(&[(r"C:\p\a.pdf", "PSV-1203", 1.0), (r"C:\p\b.pdf", "PSV-12O3", 0.95), (r"C:\p\c.pdf", "PSV-12034", 1.0), (r"C:\p\d.pdf", "PSV-1204", 1.0)]);
        let hits = find(&o, "PSV-1203", &DriveMap::default());
        let order: Vec<(&str, MatchClass)> = hits.iter().map(|h| (h.name.as_str(), h.class)).collect();
        assert_eq!(order, vec![("a.pdf", MatchClass::Exact), ("b.pdf", MatchClass::Uncertain), ("c.pdf", MatchClass::Partial)]);
        let both = find(&o, "PSV-1203 OR PSV-1204", &DriveMap::default());
        assert_eq!(both.iter().filter(|h| h.class == MatchClass::Exact).count(), 2);
    }

    const SHARE: &str = r"\\10.0.0.54\docs-latest";

    fn t_drive() -> DriveMap {
        use sleuth_core::paths::{Drive, DriveKind};
        DriveMap { drives: vec![Drive { letter: "T:".into(), kind: DriveKind::Network, unc: Some(SHARE.into()) }] }
    }

    fn names_of(o: &Office, q: &str) -> Vec<String> {
        find(o, q, &t_drive()).into_iter().map(|h| h.name).collect()
    }

    #[test]
    fn user_name_queries_find_every_file_type() {
        let iso = format!(r"{SHARE}\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf");
        let iso2 = format!(r"{SHARE}\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\ISOMETRIC ISSUANCE_26-FEB-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf");
        let o = names_only(&[
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
        assert_eq!(names_of(&o, "FSK_Batch-1")[0], "FSK_Batch-1.zip");
        assert_eq!(names_of(&o, "FSK").len(), 3);
        assert_eq!(names_of(&o, "FS").len(), 4);
        assert_eq!(names_of(&o, "1636").len(), 2);
        assert_eq!(names_of(&o, "*1636")[0], "ISO-1636.pdf");
        assert_eq!(names_of(&o, "PSV").len(), 2);
        assert_eq!(names_of(&o, "zip").len(), 2);
        // a path pasted with its mapped drive letter and spaces
        let hits = find(&o, r"T:\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf", &t_drive());
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].path, iso);
        assert_eq!(hits[0].display_path, r"T:\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf");
        assert_eq!(hits[0].kind, FileKind::Pdf);
        // quoted, too; and a piece of a path
        assert_eq!(names_of(&o, r#""T:\00_DOCUMENTS\Isometrics\Rev-1\26-02-2024\14-01-15-2632\14-01-15-2632_PSV-2809-01_HOLD.pdf""#).len(), 1);
        assert_eq!(names_of(&o, r"2632\14-01-15-2632_PSV").len(), 2);
        // a pasted folder lists what is in it
        assert_eq!(names_of(&o, r"D:\lots").len(), 2);
        // several words: AND across name and folder path
        assert_eq!(names_of(&o, "ISSUANCE PSV-2809 hold").len(), 1);
        assert_eq!(names_of(&o, "FSK zip").len(), 2);
        assert!(names_of(&o, "FSK dwg").is_empty());
    }

    #[test]
    fn exact_name_ranks_before_text_inside_drawings() {
        let mut files: Vec<(String, &str, f32)> = (0..30).map(|i| (format!(r"\\fs\p\iso{i}.pdf"), "NOTE: SEE PSV-2809 FOR SETTINGS", 1.0)).collect();
        files.push((r"\\fs\p\PSV-2809.pdf".into(), "GENERAL ARRANGEMENT", 1.0));
        let refs: Vec<(&str, &str, f32)> = files.iter().map(|(p, t, c)| (p.as_str(), *t, *c)).collect();
        let o = office_with(&refs);
        let hits = find(&o, "PSV-2809", &DriveMap::default());
        assert_eq!(hits.len(), 31);
        assert_eq!(hits[0].name, "PSV-2809.pdf");
        assert_eq!(hits[0].source, TextSource::FileName);
        assert!(hits[0].doc_id.is_some(), "found by name, and read");
    }

    #[test]
    fn filters_only_search_uses_title_block() {
        let o = office_with(&[(r"C:\p\x.pdf", "DRAWING No.: ISO-1001-01", 1.0), (r"C:\p\y.pdf", "DRAWING No.: PID-0101", 1.0)]);
        let hits = find(&o, "dwg:ISO-1001", &DriveMap::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].drawing_no.as_deref(), Some("ISO-1001-01"));
        assert_eq!(hits[0].category, sleuth_core::kinds::Category::Drawing);
        assert_eq!(find(&o, "type:drawing PID", &DriveMap::default()).len(), 1);
        // a two-letter term works with type:
        let o = office_with(&[(r"C:\p\a.pdf", "FV-2201 GLOBE", 1.0), (r"C:\p\b.pdf", "FVR NOTE 1", 1.0)]);
        let hits = find(&o, "type:instrument FV", &DriveMap::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "a.pdf", "FVR is not an instrument tag");
    }

    #[test]
    fn a_word_in_the_name_and_another_inside_is_one_row() {
        let o = office_with(&[(r"\\fs\p\Line list rev2.xlsx", "PSV-1203 | 4\" | SET 12 BARG", 1.0), (r"\\fs\p\other.xlsx", "PSV-1203", 1.0)]);
        let hits = find(&o, "line PSV-1203", &DriveMap::default());
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].name, "Line list rev2.xlsx");
        assert_eq!(hits[0].kind, FileKind::Excel);
    }

    #[test]
    fn the_same_document_in_other_types_is_listed_on_its_row() {
        let o = office_with(&[(r"\\fs\p\PDF\ISO-1001-01.pdf", "DRAWING No.: ISO-1001-01", 1.0)]);
        let cat = catalog_of(&[r"\\fs\p\PDF\ISO-1001-01.pdf", r"\\fs\p\PDF\ISO-1001-01.dwg", r"\\fs\p\DGN\iso-1001-01.dgn", r"\\fs\p\PDF\ISO-1001-02.dwg", r"\\fs\p\PDF\copy\ISO-1001-01.pdf"]);
        let o = Office { pool: o.pool, catalog: cat };
        let mut hits = find(&o, "ISO-1001-01.pdf", &DriveMap::default());
        attach_twins(&mut hits, &o.pool, &o.catalog, &DriveMap::default());
        let h = hits.iter().find(|h| h.path == r"\\fs\p\PDF\ISO-1001-01.pdf").unwrap();
        let mut kinds: Vec<(FileKind, &str)> = h.twins.iter().map(|t| (t.kind, t.why.as_str())).collect();
        kinds.sort_by_key(|k| k.1);
        assert_eq!(kinds, vec![(FileKind::Dgn, "same file name in a folder beside it"), (FileKind::Dwg, "same file name in the same folder")], "another PDF is another document; another number is not the same");
    }
}
