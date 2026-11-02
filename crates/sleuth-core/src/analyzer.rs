//! Tag-aware text matching.
//!
//! Engineering tags are written many ways (`10"-P-1001-A1B`, `10-P-1001-A1B`,
//! `10P1001A1B`) and OCR confuses look-alike characters (O/0, l/1, S/5, B/8).
//! Every string is compared in three forms, strictest first:
//!
//! 1. raw, case-insensitive substring        -> [`MatchClass::Exact`]
//! 2. separators removed (only letters/digits) -> [`MatchClass::SeparatorsIgnored`]
//! 3. look-alike characters folded            -> [`MatchClass::Uncertain`]
//!
//! A match found on text whose OCR confidence is low is never reported as
//! better than `Uncertain`: the user must verify it on the preview.

use serde::{Deserialize, Serialize};

/// Shortest separator-free query that may use look-alike folding. Folding a
/// one- or two-character query would match almost everything.
pub const MIN_FOLD_LEN: usize = 4;

/// OCR confidence below which a hit is downgraded to `Uncertain`.
pub const LOW_CONFIDENCE: f32 = 0.80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchClass {
    Exact,
    SeparatorsIgnored,
    Uncertain,
}

impl MatchClass {
    pub fn label(self) -> &'static str {
        match self {
            MatchClass::Exact => "Exact",
            MatchClass::SeparatorsIgnored => "Separators ignored",
            MatchClass::Uncertain => "Verify, OCR unsure",
        }
    }
}

/// Lower-case letters and digits only.
pub fn sep_form(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// [`sep_form`] with OCR look-alike characters mapped to one representative.
pub fn fold_form(s: &str) -> String {
    sep_form(s).chars().map(fold_char).collect()
}

fn fold_char(c: char) -> char {
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

impl Term {
    pub fn new(text: &str) -> Option<Term> {
        let raw = text.trim().to_lowercase();
        let sep = sep_form(&raw);
        if sep.is_empty() {
            return None;
        }
        let fold = fold_form(&raw);
        Some(Term { raw, sep, fold })
    }

    /// Classify this term against one piece of text (a file name or an
    /// extracted line). `confidence` is the reader's confidence in `text`,
    /// 1.0 for file names and PDF text layers.
    pub fn classify(&self, text: &str, confidence: f32) -> Option<MatchClass> {
        let low = confidence < LOW_CONFIDENCE;
        let lower = text.to_lowercase();
        if lower.contains(&self.raw) {
            return Some(if low { MatchClass::Uncertain } else { MatchClass::Exact });
        }
        if sep_form(&lower).contains(&self.sep) {
            return Some(if low { MatchClass::Uncertain } else { MatchClass::SeparatorsIgnored });
        }
        if self.sep.chars().count() >= MIN_FOLD_LEN && fold_form(&lower).contains(&self.fold) {
            return Some(MatchClass::Uncertain);
        }
        None
    }
}

/// Split a query into terms. Whitespace separates terms; a double quote opens
/// a phrase only at the start of a term, so the inch mark in `10"-P-1001`
/// stays part of the tag.
pub fn parse_query(query: &str) -> Vec<Term> {
    let mut terms = Vec::new();
    let mut chars = query.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let mut buf = String::new();
        if first == '"' {
            chars.next();
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
            }
        }
        if let Some(t) = Term::new(&buf) {
            terms.push(t);
        }
    }
    terms
}

/// Characters worth sending to the trigram index for a term: the longest of
/// the term's forms that the index stores. Returns `None` when the term is too
/// short for the trigram index and must be matched by a scan instead.
pub fn index_probe(term: &Term) -> Option<&str> {
    (term.sep.chars().count() >= 3).then_some(term.sep.as_str())
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
        assert_eq!(t.classify("PSV-12O3", 0.95), Some(MatchClass::Uncertain));
        assert_eq!(one("10-P-1001").classify("10-P-l001", 0.99), Some(MatchClass::Uncertain));
    }

    #[test]
    fn low_confidence_never_exact() {
        let t = one("PSV-1203");
        assert_eq!(t.classify("PSV-1203", 0.5), Some(MatchClass::Uncertain));
    }

    #[test]
    fn short_queries_do_not_fold() {
        assert_eq!(one("B1").classify("81", 1.0), None);
    }

    #[test]
    fn no_false_match() {
        assert_eq!(one("PSV-1203").classify("PSV-1204", 1.0), None);
        assert_eq!(one("FV-2201").classify("FV-2210", 1.0), None);
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
        assert!(parse_query("-- \"\"").is_empty());
    }
}
