//! Tag-aware text matching and the search query language.
//!
//! Engineering tags are written many ways (`10"-P-1001-A1B`, `10-P-1001-A1B`,
//! `10P1001A1B`) and OCR confuses look-alike characters (O/0, l/1, S/5, B/8).
//! Every string is compared in three forms, strictest first:
//!
//! 1. raw, case-insensitive                    -> [`MatchClass::Exact`]
//! 2. separators removed (only letters/digits) -> [`MatchClass::SeparatorsIgnored`]
//! 3. look-alike characters folded             -> [`MatchClass::Uncertain`]
//!
//! A match must stand on its own: the characters either side of it must not be
//! letters or digits. `PSV-120` inside `PSV-1203` is reported as
//! [`MatchClass::Partial`], never as exact.
//!
//! A match found on text whose OCR confidence is low is never reported as
//! better than `Uncertain`: the user must verify it on the preview.

use serde::{Deserialize, Serialize};

/// Shortest separator-free query that may use look-alike folding. Folding a
/// one- or two-character query would match almost everything.
pub const MIN_FOLD_LEN: usize = 4;

/// OCR confidence below which a hit is downgraded to `Uncertain`.
pub const LOW_CONFIDENCE: f32 = 0.80;

/// Strictest first; the order is the ranking order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchClass {
    Exact,
    SeparatorsIgnored,
    Uncertain,
    /// Found only inside a longer identifier or word.
    Partial,
}

impl MatchClass {
    pub fn label(self) -> &'static str {
        match self {
            MatchClass::Exact => "Exact",
            MatchClass::SeparatorsIgnored => "Separators ignored",
            MatchClass::Uncertain => "Verify, OCR unsure",
            MatchClass::Partial => "Partial, verify",
        }
    }
}

/// Lower-case letters and digits only.
pub fn sep_form(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// [`sep_form`] with OCR look-alike characters mapped to one representative.
pub fn fold_form(s: &str) -> String {
    sep_form(s).chars().map(fold_char).collect()
}

/// Upper-case letters and digits: the stored identity of an engineering tag.
pub fn norm_form(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_uppercase).collect()
}

pub fn fold_char(c: char) -> char {
    match c {
        'o' | 'q' | 'd' => '0',
        'i' | 'l' => '1',
        's' => '5',
        'b' => '8',
        'z' => '2',
        'g' => '6',
        other => other,
    }
}

/// One search term, already reduced to its three comparison forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub raw: String,
    pub sep: String,
    pub fold: String,
}

/// How a term matched one piece of text, with the reason shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchDetail {
    pub class: MatchClass,
    pub why: String,
}

/// The letters and digits of `text` (lower-cased), each with the char index in
/// `chars` it came from, so a match on the reduced form can be mapped back.
fn reduced(chars: &[char], fold: bool) -> (Vec<char>, Vec<usize>) {
    let mut out = Vec::new();
    let mut at = Vec::new();
    for (i, c) in chars.iter().enumerate() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                out.push(if fold { fold_char(l) } else { l });
                at.push(i);
            }
        }
    }
    (out, at)
}

/// True when the chars just outside `[start, end)` are not letters or digits.
fn stands_alone(chars: &[char], start: usize, end: usize) -> bool {
    let before = start == 0 || !chars[start - 1].is_alphanumeric();
    let after = end >= chars.len() || !chars[end].is_alphanumeric();
    before && after
}

/// Every start index of `needle` in `hay`.
fn find_all(hay: &[char], needle: &[char]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return Vec::new();
    }
    (0..=hay.len() - needle.len()).filter(|&i| hay[i..i + needle.len()] == *needle).collect()
}

impl Term {
    pub fn new(text: &str) -> Option<Term> {
        let raw = text.trim().to_lowercase();
        let sep = sep_form(&raw);
        // `_` or `&` alone is still a file-name search (literal text, see
        // [`Term::is_punctuation`]); only wildcards alone are not.
        if sep.is_empty() && raw.chars().all(|c| c == '*' || c == '?') {
            return None;
        }
        let fold = fold_form(&raw);
        Some(Term { raw, sep, fold })
    }

    /// Classify this term against one piece of text (a file name or an
    /// extracted line). `confidence` is the reader's confidence in `text`,
    /// 1.0 for file names and PDF text layers.
    pub fn classify(&self, text: &str, confidence: f32) -> Option<MatchClass> {
        self.explain(text, confidence).map(|d| d.class)
    }

    /// [`Term::classify`] with the reason in words.
    pub fn explain(&self, text: &str, confidence: f32) -> Option<MatchDetail> {
        let low = confidence < LOW_CONFIDENCE;
        let lowtxt = |what: &str| format!("{what}, but read with {}% confidence", (confidence * 100.0).round());
        let chars: Vec<char> = text.to_lowercase().chars().collect();
        let raw: Vec<char> = self.raw.chars().collect();
        let mut partial = false;
        // 1. as typed
        for i in find_all(&chars, &raw) {
            if stands_alone(&chars, i, i + raw.len()) {
                return Some(if low {
                    MatchDetail { class: MatchClass::Uncertain, why: lowtxt("Same text as typed") }
                } else {
                    MatchDetail { class: MatchClass::Exact, why: "Same text as typed".into() }
                });
            }
            partial = true;
        }
        // 2. separators ignored
        let (sep, at) = reduced(&chars, false);
        let want: Vec<char> = self.sep.chars().collect();
        for i in find_all(&sep, &want) {
            let (s, e) = (at[i], at[i + want.len() - 1] + 1);
            if stands_alone(&chars, s, e) {
                let seen: String = chars[s..e].iter().collect();
                return Some(if low {
                    MatchDetail { class: MatchClass::Uncertain, why: lowtxt(&format!("Same letters and digits as \"{seen}\"")) }
                } else {
                    MatchDetail { class: MatchClass::SeparatorsIgnored, why: format!("Same letters and digits; written \"{seen}\"") }
                });
            }
            partial = true;
        }
        // 3. look-alike characters
        if want.len() >= MIN_FOLD_LEN {
            let (fold, at) = reduced(&chars, true);
            let wantf: Vec<char> = self.fold.chars().collect();
            for i in find_all(&fold, &wantf) {
                let (s, e) = (at[i], at[i + wantf.len() - 1] + 1);
                if stands_alone(&chars, s, e) {
                    let seen: String = chars[s..e].iter().collect();
                    let diffs = lookalike_diffs(&sep[i..i + want.len()], &want);
                    return Some(MatchDetail { class: MatchClass::Uncertain, why: format!("Read as \"{seen}\"{diffs}; verify on the page") });
                }
                partial = true;
            }
        }
        if partial {
            return Some(MatchDetail { class: MatchClass::Partial, why: "Only inside a longer tag or word".into() });
        }
        // 4. a reading missing a few characters at one end (text crossed by a
        //    line, cut by the sheet edge, or faint)
        if let Some(seen) = self.truncated_in(text) {
            return Some(MatchDetail { class: MatchClass::Partial, why: format!("Only part of the tag was read (\"{seen}\"); the rest may be hidden by a line or not read. Verify on the page") });
        }
        None
    }

    /// Shortest reading accepted as part of this tag.
    fn min_truncated(&self) -> usize {
        let n = self.sep.chars().count();
        (n * 7).div_ceil(10).max(TRUNCATED_MIN)
    }

    /// A whitespace-separated word of `text` that is this tag with a few
    /// characters missing at the start or the end.
    fn truncated_in(&self, text: &str) -> Option<String> {
        let n = self.sep.chars().count();
        if n < TRUNCATED_MIN + 1 {
            return None;
        }
        let min = self.min_truncated();
        text.split(|c: char| c.is_whitespace() || c == ',' || c == ';').find_map(|w| {
            let s = sep_form(w);
            let k = s.chars().count();
            (k >= min && k < n && s.chars().any(|c| c.is_ascii_digit()) && (self.sep.starts_with(&s) || self.sep.ends_with(&s))).then(|| w.trim().to_string())
        })
    }

    /// Folded forms the index is asked for to find readings missing a few
    /// characters at either end (see [`Term::explain`], case 4).
    pub fn truncations(&self) -> Vec<String> {
        let chars: Vec<char> = self.fold.chars().collect();
        let n = chars.len();
        if n < TRUNCATED_MIN + 1 {
            return Vec::new();
        }
        let min = self.min_truncated();
        let mut out = Vec::new();
        for k in min..n {
            out.push(chars[n - k..].iter().collect());
            out.push(chars[..k].iter().collect());
        }
        out
    }
}

/// File-name and path matching, the way Everything (voidtools) does it:
/// case-insensitive substring of the name, `*` and `?` wildcards, and a term
/// containing `\` compared with the full path.
impl Term {
    /// Written with `*` or `?`.
    pub fn has_wildcards(&self) -> bool {
        self.raw.contains(['*', '?'])
    }

    /// Contains `\`: compared with the full path, not the name.
    pub fn is_path(&self) -> bool {
        self.raw.contains('\\')
    }

    /// The longest run of the term without wildcards (the whole term when it
    /// has none); what the index is asked for.
    pub fn literal(&self) -> &str {
        self.raw.split(['*', '?']).max_by_key(|s| sep_form(s).chars().count()).unwrap_or("")
    }

    /// How this term matches a file. A path term (`\` in it) is compared with
    /// `path`; any other term with `name`. Wildcards must match the whole
    /// name or path for [`MatchClass::Exact`]; matching only a part of it is
    /// [`MatchClass::Partial`]. Look-alike folding never applies to names.
    pub fn explain_name(&self, name: &str, path: &str) -> Option<MatchDetail> {
        let target = if self.is_path() { path } else { name };
        let what = if self.is_path() { "path" } else { "name" };
        if self.has_wildcards() {
            let t = target.to_lowercase();
            let stem = if self.is_path() { None } else { t.rsplit_once('.').map(|(s, _)| s.to_string()) };
            if glob_match(&self.raw, &t) || stem.is_some_and(|s| glob_match(&self.raw, &s)) {
                return Some(MatchDetail { class: MatchClass::Exact, why: format!("The {what} matches the pattern") });
            }
            if glob_match(&format!("*{}*", self.raw), &t) {
                return Some(MatchDetail { class: MatchClass::Partial, why: format!("Part of the {what} matches the pattern") });
            }
            return None;
        }
        if self.is_literal_only() {
            return self.literal_in(target);
        }
        // Typed punctuation at an end anchors the match there: `_ACTIVIT` is
        // exact in `PLAN_ACTIVIT_A.pdf`.
        if !self.is_path()
            && let Some(d) = self.literal_in(target)
            && d.class == MatchClass::Exact
        {
            return Some(d);
        }
        // `Rev-1\` and `\26-02-2024` mark folder boundaries themselves.
        let trimmed = if self.is_path() { Term::new(self.raw.trim_matches('\\')).filter(|_| !self.raw.starts_with("\\\\")) } else { None };
        let d = trimmed.as_ref().unwrap_or(self).explain(target, 1.0)?;
        (d.class != MatchClass::Uncertain).then_some(d)
    }

    /// No letters or digits at all (`_`, `&`).
    pub fn is_punctuation(&self) -> bool {
        self.sep.is_empty()
    }

    /// Matched only as typed: punctuation alone, or fewer than three letters
    /// and digits written with punctuation (`-1`, `_A`), where dropping the
    /// punctuation would match almost every name.
    pub fn is_literal_only(&self) -> bool {
        !self.has_wildcards() && self.sep.chars().count() < 3 && self.sep != self.raw.chars().filter(|c| !c.is_whitespace()).collect::<String>()
    }

    /// [`Term::explain_name`] for [`Term::is_literal_only`] terms: the text
    /// as typed (any case). Exact when no letter or digit continues it on a
    /// side where the term itself ends in a letter or digit (`-1` in
    /// `PSV-1.pdf`, not in `PSV-12.pdf`).
    fn literal_in(&self, text: &str) -> Option<MatchDetail> {
        let chars: Vec<char> = text.to_lowercase().chars().collect();
        let raw: Vec<char> = self.raw.chars().collect();
        let (first, last) = (raw.first()?.is_alphanumeric(), raw.last()?.is_alphanumeric());
        let hits = find_all(&chars, &raw);
        if hits.is_empty() {
            return None;
        }
        let alone = hits.iter().any(|&i| {
            let before = !first || i == 0 || !chars[i - 1].is_alphanumeric();
            let end = i + raw.len();
            let after = !last || end >= chars.len() || !chars[end].is_alphanumeric();
            before && after
        });
        Some(if alone {
            MatchDetail { class: MatchClass::Exact, why: "Same text as typed".into() }
        } else {
            MatchDetail { class: MatchClass::Partial, why: "Only inside a longer tag or word".into() }
        })
    }

    /// The name is the term itself, with or without its extension (or, for a
    /// path term, the path is the term).
    pub fn is_whole_name(&self, name: &str, path: &str) -> bool {
        if self.has_wildcards() {
            return false;
        }
        if self.is_path() {
            return path.to_lowercase().trim_end_matches('\\') == self.raw.trim_end_matches('\\');
        }
        let n = name.to_lowercase();
        n == self.raw || n.rsplit_once('.').is_some_and(|(stem, _)| stem == self.raw)
    }
}

/// Case-sensitive glob (callers lower-case both sides): `*` any run of
/// characters, `?` exactly one character, everything else literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || (p[pi] != '*' && p[pi] == t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// A pasted absolute path: `T:\folder\file.pdf` or `\\server\share\...`.
pub fn looks_like_path(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with("\\\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\')
}

/// A tag reading shorter than this is never offered as part of a tag.
pub const TRUNCATED_MIN: usize = 5;

/// ` (O for 0, l for 1)` describing which look-alikes differ.
fn lookalike_diffs(seen: &[char], want: &[char]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (a, b) in seen.iter().zip(want) {
        if a != b {
            let p = format!("{} for {}", a.to_uppercase(), b.to_uppercase());
            if !parts.contains(&p) {
                parts.push(p);
            }
        }
    }
    if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) }
}

/// Split plain text into terms. Whitespace separates terms; a double quote
/// opens a phrase only at the start of a term, so the inch mark in
/// `10"-P-1001` stays part of the tag.
pub fn parse_query(query: &str) -> Vec<Term> {
    tokens(query).into_iter().filter_map(|t| Term::new(&t.text)).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    text: String,
    /// Written inside quotes: never an operator or filter.
    quoted: bool,
}

fn tokens(query: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = query.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let mut buf = String::new();
        let mut quoted = false;
        if first == '"' {
            chars.next();
            quoted = true;
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                buf.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                buf.push(c);
                chars.next();
                // name:"rev c" -> the quoted value belongs to the filter
                if c == ':' && chars.peek() == Some(&'"') && is_filter_key(&buf[..buf.len() - 1]) {
                    chars.next();
                    for q in chars.by_ref() {
                        if q == '"' {
                            break;
                        }
                        buf.push(q);
                    }
                    break;
                }
            }
        }
        out.push(Token { text: buf, quoted });
    }
    out
}

fn is_filter_key(k: &str) -> bool {
    matches!(k.to_ascii_lowercase().as_str(), "name" | "rev" | "dwg" | "disc" | "type" | "ext")
}

/// Restrictions written into the query with `key:value`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    /// Terms that must all be in the file name.
    pub name: Vec<Term>,
    /// Revision from the title block, compared with separators ignored.
    pub rev: Option<String>,
    /// Drawing number from the title block contains this (separators ignored).
    pub dwg: Option<String>,
    /// Discipline, case-insensitive.
    pub disc: Option<String>,
    /// Tag category (`line`, `instrument`, ...): matches must be such a tag.
    pub tag_type: Option<String>,
    /// File extension without the dot, lower case.
    pub ext: Option<String>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        *self == Filters::default()
    }
    /// Filters that need the drawing metadata table.
    pub fn needs_metadata(&self) -> bool {
        self.rev.is_some() || self.dwg.is_some() || self.disc.is_some()
    }
}

/// A parsed search: any one of `groups` matches (OR), and within a group every
/// term must match the same file (AND). `filters` apply to every group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub groups: Vec<Vec<Term>>,
    pub filters: Filters,
}

impl SearchQuery {
    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|g| g.is_empty()) && self.filters.is_empty()
    }
    /// Every term of every group, for marking matches on a page.
    pub fn all_terms(&self) -> Vec<Term> {
        let mut v: Vec<Term> = self.groups.iter().flatten().cloned().collect();
        v.extend(self.filters.name.iter().cloned());
        v
    }
}

/// Parse the search box: `PSV-1203 OR PSV-1204`, `10"-P-1001 A1B`,
/// `name:datasheet rev:C type:instrument`. `OR` (upper case) and `|` separate
/// alternatives; a quoted `"OR"` is searched as text.
pub fn parse_search(query: &str) -> SearchQuery {
    let mut q = SearchQuery::default();
    // A pasted full path keeps its spaces: it is one term, not several words.
    let whole = query.trim();
    let whole = whole.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(whole);
    if looks_like_path(whole) && !whole.contains('"') && !whole.split_whitespace().any(|w| w == "OR" || w == "|")
        && let Some(t) = Term::new(whole)
    {
        q.groups.push(vec![t]);
        return q;
    }
    let mut group: Vec<Term> = Vec::new();
    for t in tokens(query) {
        if !t.quoted && (t.text == "OR" || t.text == "|") {
            if !group.is_empty() {
                q.groups.push(std::mem::take(&mut group));
            }
            continue;
        }
        if !t.quoted
            && let Some((k, v)) = t.text.split_once(':')
            && is_filter_key(k)
            && !v.trim().is_empty()
        {
            let v = v.trim().to_string();
            match k.to_ascii_lowercase().as_str() {
                "name" => q.filters.name.extend(parse_query(&format!("\"{v}\""))),
                "rev" => q.filters.rev = Some(v),
                "dwg" => q.filters.dwg = Some(v),
                "disc" => q.filters.disc = Some(v.to_lowercase()),
                "type" => q.filters.tag_type = Some(v.to_lowercase()),
                "ext" => q.filters.ext = Some(v.trim_start_matches('.').to_lowercase()),
                _ => {}
            }
            continue;
        }
        if let Some(term) = Term::new(&t.text) {
            group.push(term);
        }
    }
    if !group.is_empty() {
        q.groups.push(group);
    }
    // A lone `-` or `&` between words (`PSV - 1203`, `A & B`) is spacing,
    // not a search of its own; punctuation is searched only when nothing
    // else was typed in that alternative.
    for g in &mut q.groups {
        if g.iter().any(|t| !t.is_punctuation()) {
            g.retain(|t| !t.is_punctuation());
        }
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(q: &str) -> Term {
        let mut t = parse_query(q);
        assert_eq!(t.len(), 1, "query {q:?} should be one term");
        t.remove(0)
    }

    #[test]
    fn forms() {
        assert_eq!(sep_form("10\"-P-1001-A1B"), "10p1001a1b");
        assert_eq!(norm_form("10\"-p-1001-a1b"), "10P1001A1B");
        assert_eq!(fold_form("PSV-12O3"), fold_form("PSV-1203"));
        assert_eq!(fold_form("10-P-l001"), fold_form("10-P-1001"));
    }

    #[test]
    fn exact_beats_separator() {
        let t = one("10\"-P-1001-A1B");
        assert_eq!(t.classify("LINE 10\"-P-1001-A1B CS", 1.0), Some(MatchClass::Exact));
        assert_eq!(t.classify("10-P-1001-A1B", 1.0), Some(MatchClass::SeparatorsIgnored));
        assert_eq!(t.classify("10P1001A1B", 1.0), Some(MatchClass::SeparatorsIgnored));
    }

    #[test]
    fn lookalikes_are_uncertain() {
        let t = one("PSV-1203");
        let d = t.explain("PSV-12O3", 0.95).unwrap();
        assert_eq!(d.class, MatchClass::Uncertain);
        assert!(d.why.contains("O for 0"), "{}", d.why);
        assert_eq!(one("10-P-1001").classify("10-P-l001", 0.99), Some(MatchClass::Uncertain));
    }

    #[test]
    fn low_confidence_never_exact() {
        let t = one("PSV-1203");
        assert_eq!(t.classify("PSV-1203", 0.5), Some(MatchClass::Uncertain));
        assert!(t.explain("PSV-1203", 0.5).unwrap().why.contains("50%"));
    }

    #[test]
    fn short_queries_do_not_fold() {
        assert_eq!(one("B1").classify("81", 1.0), None);
    }

    #[test]
    fn no_false_match() {
        assert_eq!(one("PSV-1204").classify("PSV-1203", 1.0), None);
        assert_eq!(one("FV-2201").classify("FV-2210", 1.0), None);
    }

    #[test]
    fn part_of_a_longer_tag_is_never_exact() {
        assert_eq!(one("PSV-120").classify("PSV-1203", 1.0), Some(MatchClass::Partial));
        assert_eq!(one("P-1001").classify("P-10012-A1B", 1.0), Some(MatchClass::Partial));
        // a whole segment of a line number stands alone
        assert_eq!(one("P-1001").classify("10\"-P-1001-A1B", 1.0), Some(MatchClass::Exact));
        assert_eq!(one("10P1001").classify("10\"-P-1001-A1B", 1.0), Some(MatchClass::SeparatorsIgnored));
        // a later standalone copy wins over an earlier partial one
        assert_eq!(one("PSV-120").classify("PSV-1203 / PSV-120", 1.0), Some(MatchClass::Exact));
    }

    #[test]
    fn reading_with_a_hidden_start_is_partial_never_exact() {
        let t = one("4\"-P-1002-A1B");
        let d = t.explain("-P-1002-A1B", 1.0).unwrap();
        assert_eq!(d.class, MatchClass::Partial);
        assert!(d.why.contains("Only part of the tag"), "{}", d.why);
        assert_eq!(t.explain("4\"-P-1002-A", 1.0).unwrap().class, MatchClass::Partial, "end missing");
        // too little of the tag: not offered
        assert_eq!(t.classify("P-1002", 1.0), None);
        // short tags are never matched by a fragment
        assert_eq!(one("PSV-12").classify("SV-12", 1.0), None);
        assert!(t.truncations().contains(&"p1002a18".to_string()));
    }

    #[test]
    fn inch_mark_stays_in_tag() {
        let terms = parse_query("10\"-P-1001-A1B  \"rev c\"");
        assert_eq!(terms.len(), 2);
        assert_eq!(terms[0].raw, "10\"-p-1001-a1b");
        assert_eq!(terms[1].raw, "rev c");
    }

    #[test]
    fn empty_and_punctuation_queries() {
        assert!(parse_query("   ").is_empty());
        assert!(parse_query("\"\" * ??").is_empty());
        assert!(parse_search("  ").is_empty());
        // punctuation alone is a (file-name) search, as in Everything
        let t = &parse_query("_")[0];
        assert!(t.is_punctuation() && t.is_literal_only());
        assert_eq!(t.explain_name("a_b.pdf", "").unwrap().class, MatchClass::Exact);
        assert!(t.explain_name("ab.pdf", "").is_none());
        assert_eq!(parse_query("& --").len(), 2);
        assert_eq!(parse_search("& --").groups[0].len(), 2);
        assert_eq!(parse_search("PSV - 1203").groups[0].len(), 2, "a spaced dash is not a term");
        // a short term with punctuation needs that punctuation
        let t = &parse_query("-1")[0];
        assert!(t.is_literal_only());
        assert_eq!(t.explain_name("PSV-1.pdf", "").unwrap().class, MatchClass::Exact);
        assert_eq!(t.explain_name("PSV-12.pdf", "").unwrap().class, MatchClass::Partial);
        assert!(t.explain_name("PSV1.pdf", "").is_none());
        assert!(t.explain_name("1.pdf", "").is_none());
        assert!(!parse_query("ab")[0].is_literal_only());
    }

    #[test]
    fn or_groups_and_filters() {
        let q = parse_search("PSV-1203 OR PSV-1204 | FV 2201 name:\"rev c\" type:Instrument rev:C");
        assert_eq!(q.groups.len(), 3);
        assert_eq!(q.groups[2].len(), 2);
        assert_eq!(q.filters.name[0].raw, "rev c");
        assert_eq!(q.filters.tag_type.as_deref(), Some("instrument"));
        assert_eq!(q.filters.rev.as_deref(), Some("C"));
        // lower-case "or" and a quoted OR are text
        assert_eq!(parse_search("valve or \"OR\"").groups[0].len(), 3);
        // a colon inside a tag is not a filter
        assert_eq!(parse_search("REF:1001").groups[0][0].raw, "ref:1001");
    }

    #[test]
    fn pasted_path_is_one_term() {
        let p = r"T:\00_DOCUMENTS\Rev-1\14-01-15-2632_PSV-2809-01_HOLD.pdf";
        for q in [p.to_string(), format!("\"{p}\""), format!("  {p} ")] {
            let s = parse_search(&q);
            assert_eq!(s.groups, vec![vec![Term::new(p).unwrap()]], "{q}");
            assert!(s.groups[0][0].is_path());
        }
        assert_eq!(parse_search(r"\\fs01\share\a b.pdf").groups[0].len(), 1);
        // words that merely contain a backslash stay words
        assert_eq!(parse_search(r"Rev-1\ PSV").groups[0].len(), 2);
    }

    #[test]
    fn names_wildcards_and_paths() {
        assert!(glob_match("*1636", "iso-1636"));
        assert!(glob_match("fsk_batch-?.zip", "fsk_batch-1.zip"));
        assert!(!glob_match("fsk_batch-?.zip", "fsk_batch-12.zip"));
        assert!(glob_match("a*b*c", "axxbyyc") && !glob_match("a*b*c", "axxbyy"));
        let t = one("*1636");
        assert!(t.has_wildcards());
        assert_eq!(t.literal(), "1636");
        assert_eq!(t.explain_name("ISO-1636.pdf", "").unwrap().class, MatchClass::Exact);
        assert_eq!(t.explain_name("A-1636-REV.2.xlsx", "").unwrap().class, MatchClass::Partial);
        assert!(t.explain_name("A-1637.pdf", "").is_none());
        let t = one("FSK_Batch-1");
        assert!(t.is_whole_name("FSK_Batch-1.zip", "") && t.is_whole_name("fsk_batch-1", ""));
        assert!(!t.is_whole_name("FSK_Batch-12.zip", ""));
        assert_eq!(t.explain_name("FSK Batch 1.zip", "").unwrap().class, MatchClass::SeparatorsIgnored);
        // names are never look-alike matches
        assert!(one("PSV-1203").explain_name("PSV-12O3.pdf", "").is_none());
        let t = one(r"Rev-1\26-02-2024\");
        assert_eq!(t.explain_name("x.pdf", r"\\s\h\Rev-1\26-02-2024\x.pdf").unwrap().class, MatchClass::Exact);
        assert!(t.explain_name("Rev-1 26-02-2024.pdf", r"C:\Rev-1 26-02-2024.pdf").is_none_or(|d| d.class != MatchClass::Exact));
        assert!(looks_like_path(r"C:\x") && looks_like_path(r"\\srv\s") && !looks_like_path(r"PSV\x") && !looks_like_path("C:"));
    }
}
