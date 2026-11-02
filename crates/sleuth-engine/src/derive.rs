//! Deterministic engineering data taken from the text of a drawing: tags
//! (line numbers, instruments, valves, equipment...) found by configurable
//! patterns, and title-block details found next to their labels.
//!
//! Nothing here guesses or corrects: a tag is stored exactly as read, with the
//! reading's confidence; a title-block value is stored with the line it came
//! from, so the user can check it. Rules come from settings; the hash of the
//! rules is stored with every derived row, so changed rules re-derive from the
//! stored text without OCR.

use regex::{Regex, RegexBuilder};
use sleuth_core::analyzer::norm_form;
use sleuth_core::protocol::{DrawingMeta, NormBox, TextSource};
use sleuth_core::settings::Settings;

use crate::text::Line;

/// Bump when the parsing code (not the rules) changes.
pub const PARSER_VERSION: u32 = 2;

/// Words that look like tag prefixes but are labels.
const STOP: &[&str] = &[
    "LINE", "REV", "SHEET", "PAGE", "NOTE", "ITEM", "SIZE", "TYPE", "PART", "FIG", "AREA", "UNIT", "PLOT", "SCALE", "DATE", "QTY", "NO", "REF", "ZONE", "TAG", "SPEC",
    "CLASS", "DWG", "DRG", "DOC", "ISO", "PO", "JOB", "TEL", "FAX", "PIN", "SH",
];

/// Status words recognised anywhere on page 1 when no status label is found.
const STATUS_WORDS: &[&str] = &["AS-BUILT", "AS BUILT", "FOR CONSTRUCTION", "ISSUED FOR CONSTRUCTION", "FOR APPROVAL", "FOR REVIEW", "PRELIMINARY", "SUPERSEDED", "VOID", "FOR INFORMATION"];

/// Title blocks sit at the bottom-right of a sheet.
const TITLE_BLOCK: NormBox = NormBox { x: 0.45, y: 0.55, w: 0.55, h: 0.45 };

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundTag {
    pub category: String,
    /// Exactly as printed (or read).
    pub raw: String,
    /// Upper-case letters and digits.
    pub norm: String,
}

struct Field {
    name: String,
    labels: Vec<Regex>,
    value: Regex,
}

pub struct Deriver {
    tags: Vec<(String, Regex)>,
    fields: Vec<Field>,
    prefixes: Vec<(String, String)>,
    revision_from_name: Regex,
    /// Identifies the rules and parser version; stored with derived rows.
    pub hash: String,
    /// Identifies only the title-block rules: they decide whether a plotted
    /// drawing's title block is read with OCR, so they are part of the OCR
    /// pipeline fingerprint (tag rules are not).
    pub fields_hash: String,
    /// Rules that could not be used, with the reason.
    pub errors: Vec<String>,
}

impl Deriver {
    pub fn new(s: &Settings) -> Deriver {
        let mut errors = Vec::new();
        let build = |p: &str| RegexBuilder::new(p).case_insensitive(true).size_limit(1 << 20).build();
        let mut tags = Vec::new();
        let rules = s.tag_rules();
        for r in rules.iter().filter(|r| r.enabled) {
            match build(&r.pattern) {
                Ok(re) => tags.push((r.category.trim().to_lowercase(), re)),
                Err(e) => errors.push(format!("tag rule {} ({}): {e}", r.category, r.pattern)),
            }
        }
        let mut fields = Vec::new();
        let frules = s.field_rules();
        for f in &frules {
            let labels: Vec<Regex> = f
                .labels
                .iter()
                .filter_map(|l| {
                    // "DRAWING NO" also matches "DRAWING No.:" and "DRAWING NUMBER"; spaces may vary.
                    let words: Vec<String> = l.split_whitespace().map(regex::escape).collect();
                    build(&format!(r"(?:^|[^A-Z0-9]){}\.?(?:[^A-Z0-9]|$)", words.join(r"[\s.]*"))).map_err(|e| errors.push(format!("label {l}: {e}"))).ok()
                })
                .collect();
            match build(&format!(r"^(?:{})", f.value)) {
                Ok(value) => fields.push(Field { name: f.field.clone(), labels, value }),
                Err(e) => errors.push(format!("field {}: {e}", f.field)),
            }
        }
        let prefixes = s.discipline_prefixes();
        let fields_hash = blake3::hash(format!("{PARSER_VERSION}{}", serde_json::to_string(&frules).unwrap_or_default()).as_bytes()).to_hex()[..16].to_string();
        let mut h = blake3::Hasher::new();
        h.update(format!("parser={PARSER_VERSION}").as_bytes());
        h.update(serde_json::to_string(&rules).unwrap_or_default().as_bytes());
        h.update(serde_json::to_string(&frules).unwrap_or_default().as_bytes());
        h.update(serde_json::to_string(&prefixes).unwrap_or_default().as_bytes());
        Deriver {
            tags,
            fields,
            prefixes,
            revision_from_name: Regex::new(r"(?i)\brev(?:ision)?[ ._-]*([A-Z0-9]{1,2})\b").unwrap(),
            hash: h.finalize().to_hex()[..16].to_string(),
            fields_hash,
            errors,
        }
    }

    /// Tags in one line of text. Rules are tried in order; a later rule never
    /// takes characters an earlier rule already matched (so a line number is
    /// not also reported as equipment `P-1001`).
    pub fn tags(&self, text: &str) -> Vec<FoundTag> {
        let mut taken: Vec<(usize, usize)> = Vec::new();
        let mut out = Vec::new();
        for (category, re) in &self.tags {
            for c in re.captures_iter(text) {
                let m = c.get(1).or_else(|| c.get(0)).unwrap();
                let whole = c.get(0).unwrap();
                if taken.iter().any(|(s, e)| whole.start() < *e && *s < whole.end()) {
                    continue;
                }
                // Never a fragment of a longer hyphenated identifier: `CW-2014`
                // in `6'-CW-2014-B2A` is part of a line number, not equipment.
                let before = &text[..whole.start()];
                let after = &text[whole.end()..];
                let joined_before = before.ends_with('-') && before[..before.len() - 1].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '"' || c == '\'');
                let joined_after = after.starts_with('-') && after[1..].chars().next().is_some_and(char::is_alphanumeric);
                if (joined_before || joined_after) && category != "line" && category != "drawing" {
                    continue;
                }
                let raw = m.as_str().trim();
                let norm = norm_form(raw);
                if norm.len() < 3 || !norm.chars().any(|c| c.is_ascii_digit()) {
                    continue;
                }
                let letters: String = norm.chars().take_while(|c| c.is_alphabetic()).collect();
                if STOP.contains(&letters.as_str()) && category != "drawing" && category != "spec" {
                    continue;
                }
                taken.push((whole.start(), whole.end()));
                out.push(FoundTag { category: category.clone(), raw: raw.to_string(), norm });
            }
        }
        out
    }

    /// Does this page text already state a drawing number?
    pub fn has_drawing_number(&self, lines: &[Line]) -> bool {
        let refs: Vec<(u32, &Line)> = lines.iter().map(|l| (1u32, l)).collect();
        self.field_value("drawing_number", &refs).is_some()
    }

    /// Title-block details from page 1 (and the file name as a fallback).
    pub fn metadata(&self, file_name: &str, lines: &[(u32, &Line)]) -> DrawingMeta {
        let first: Vec<(u32, &Line)> = lines.iter().filter(|(p, _)| *p == 1).copied().collect();
        let mut m = DrawingMeta::default();
        for f in &self.fields {
            if let Some((value, evidence)) = self.field_value(&f.name, &first) {
                m.evidence.push((f.name.clone(), evidence));
                match f.name.as_str() {
                    "drawing_number" => m.drawing_no = Some(value),
                    "revision" => m.revision = Some(value),
                    "title" => m.title = Some(value),
                    "project" => m.project = Some(value),
                    "sheet" => m.sheet = Some(value),
                    "discipline" => m.discipline = Some(value),
                    "status" => m.status = Some(value),
                    _ => {}
                }
            }
        }
        let stem = file_name.rsplit_once('.').map(|(a, _)| a).unwrap_or(file_name);
        if m.drawing_no.is_none()
            && let Some(t) = self.tags(stem).into_iter().find(|t| t.category == "drawing")
        {
            m.drawing_no = Some(t.raw.to_uppercase());
            m.evidence.push(("drawing_number".into(), format!("file name \"{file_name}\"")));
        }
        if m.revision.is_none()
            && let Some(c) = self.revision_from_name.captures(stem)
        {
            m.revision = Some(c[1].to_uppercase());
            m.evidence.push(("revision".into(), format!("file name \"{file_name}\"")));
        }
        if m.discipline.is_none()
            && let Some(d) = &m.drawing_no
        {
            let up = d.to_uppercase();
            let fits = |p: &str| up.strip_prefix(&p.to_uppercase()).is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphabetic()));
            if let Some((p, disc)) = self.prefixes.iter().find(|(p, _)| fits(p)) {
                m.discipline = Some(disc.clone());
                m.evidence.push(("discipline".into(), format!("drawing number starts with {p}")));
            }
        }
        if m.status.is_none() {
            'status: for (_, l) in &first {
                let up = l.text.to_uppercase();
                for w in STATUS_WORDS {
                    if up.contains(w) {
                        m.status = Some(w.to_string());
                        m.evidence.push(("status".into(), format!("page 1: \"{}\"", l.text)));
                        break 'status;
                    }
                }
            }
        }
        m
    }

    /// Find one field: a label, then its value on the same line after the
    /// label, or on the line just right of it, or just below it. Lines in the
    /// title-block area are tried first.
    fn field_value(&self, name: &str, lines: &[(u32, &Line)]) -> Option<(String, String)> {
        let f = self.fields.iter().find(|f| f.name == name)?;
        let mut order: Vec<&(u32, &Line)> = lines.iter().collect();
        order.sort_by_key(|(_, l)| if TITLE_BLOCK.overlap(&l.bbox) > 0.5 { 0 } else { 1 });
        for (page, l) in order {
            let text = l.text.trim();
            for label in &f.labels {
                let Some(m) = label.find(text) else { continue };
                let rest = text[m.end()..].trim_start_matches(|c: char| c == ':' || c == '.' || c == '#' || c == '-' || c.is_whitespace());
                let (value, from) = if !rest.is_empty() && f.value.is_match(rest) {
                    (rest, *l)
                } else if let Some(n) = neighbour(l, lines, &f.value, &|t| self.is_label(t)) {
                    (n.text.trim(), n)
                } else {
                    continue;
                };
                let Some(v) = f.value.find(value) else { continue };
                let v = v.as_str().trim().trim_end_matches([',', ';']).to_string();
                if v.is_empty() || (name == "drawing_number" && !v.chars().any(|c| c.is_ascii_digit())) {
                    continue;
                }
                let how = match from.source {
                    TextSource::Ocr => format!(", OCR {}%", (from.conf * 100.0).round()),
                    TextSource::Verified => ", checked by a person".into(),
                    _ => String::new(),
                };
                return Some((v, format!("page {page}: \"{}\"{how}", from.text.trim())));
            }
        }
        None
    }
}

impl Deriver {
    /// Text that holds a label of any field (so it is not a value).
    fn is_label(&self, text: &str) -> bool {
        self.fields.iter().any(|f| f.labels.iter().any(|r| r.is_match(text)))
    }
}

/// The nearest line to the right on the same row, else the nearest below,
/// whose text starts with a value and is not itself a label.
fn neighbour<'a>(l: &Line, lines: &'a [(u32, &'a Line)], value: &Regex, is_label: &dyn Fn(&str) -> bool) -> Option<&'a Line> {
    let cy = l.bbox.y + l.bbox.h / 2.0;
    let h = l.bbox.h.max(0.004);
    let right = lines
        .iter()
        .map(|(_, n)| *n)
        .filter(|n| ((n.bbox.y + n.bbox.h / 2.0) - cy).abs() < h * 0.6 && n.bbox.x > l.bbox.x + l.bbox.w - h && n.bbox.x - (l.bbox.x + l.bbox.w) < h * 12.0)
        .filter(|n| value.is_match(n.text.trim()) && !is_label(&n.text))
        .min_by(|a, b| a.bbox.x.total_cmp(&b.bbox.x));
    if right.is_some() {
        return right;
    }
    lines
        .iter()
        .map(|(_, n)| *n)
        .filter(|n| n.bbox.y > l.bbox.y + h * 0.5 && n.bbox.y - (l.bbox.y + l.bbox.h) < h * 2.5)
        .filter(|n| n.bbox.x < l.bbox.x + l.bbox.w + h * 4.0 && n.bbox.x + n.bbox.w > l.bbox.x - h * 2.0)
        .filter(|n| value.is_match(n.text.trim()) && !is_label(&n.text))
        .min_by(|a, b| a.bbox.y.total_cmp(&b.bbox.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d() -> Deriver {
        let d = Deriver::new(&Settings::default());
        assert!(d.errors.is_empty(), "{:?}", d.errors);
        d
    }

    fn cats(text: &str) -> Vec<(String, String)> {
        d().tags(text).into_iter().map(|t| (t.category, t.norm)).collect()
    }

    #[test]
    fn piping_tags() {
        assert_eq!(cats("LINE 6\"-P-1001-A1B"), vec![("line".into(), "6P1001A1B".into())]);
        assert_eq!(cats("PSV-1203 SET 12 BARG"), vec![("instrument".into(), "PSV1203".into())]);
        assert_eq!(cats("FV 2201"), vec![("instrument".into(), "FV2201".into())]);
        assert_eq!(cats("DRAWING No.: ISO-1001-01"), vec![("drawing".into(), "ISO100101".into())]);
        assert_eq!(cats("PUMP P-101A"), vec![("equipment".into(), "P101A".into())]);
        // OCR reads the inch mark as an apostrophe: still one line number, no fragments
        assert_eq!(cats("6'-CW-2014-B2A"), vec![("line".into(), "6CW2014B2A".into())]);
        assert!(cats("X-CW-2014-B2A").iter().all(|(c, _)| c != "equipment"));
        assert_eq!(cats("SPEC: A1B"), vec![("spec".into(), "A1B".into())]);
        // labels are not tags
        assert!(cats("LINE 1001").is_empty());
        assert!(cats("SHEET 1 OF 1").is_empty());
    }

    fn l(text: &str, x: f32, y: f32, w: f32) -> Line {
        Line { text: text.into(), conf: 1.0, source: TextSource::TextLayer, bbox: NormBox { x, y, w, h: 0.012 }, alt: false }
    }

    #[test]
    fn title_block_fields() {
        let lines = vec![
            l("SAMPLE ENGINEERING PROJECT", 0.62, 0.84, 0.2),
            l("DRG. TITLE: PIPING ISOMETRIC & BOM", 0.62, 0.87, 0.25),
            l("DRAWING No.: ISO-1001-01", 0.62, 0.9, 0.18),
            l("REV. A", 0.82, 0.9, 0.05),
            l("SHEET 1 OF 1", 0.82, 0.94, 0.08),
            l("AS-BUILT", 0.1, 0.1, 0.05),
        ];
        let refs: Vec<(u32, &Line)> = lines.iter().map(|x| (1, x)).collect();
        let m = d().metadata("ISO-1001-01.pdf", &refs);
        assert_eq!(m.drawing_no.as_deref(), Some("ISO-1001-01"));
        assert_eq!(m.revision.as_deref(), Some("A"));
        assert_eq!(m.title.as_deref(), Some("PIPING ISOMETRIC & BOM"));
        assert_eq!(m.sheet.as_deref(), Some("1 OF 1"));
        assert_eq!(m.discipline.as_deref(), Some("Piping"));
        assert_eq!(m.status.as_deref(), Some("AS-BUILT"));
        assert_eq!(m.project, None, "a heading ending in PROJECT has no value; the title line below is a label");
        assert!(m.evidence.iter().any(|(f, e)| f == "drawing_number" && e.contains("ISO-1001-01")));
    }

    #[test]
    fn value_below_its_label_and_file_name_fallback() {
        let lines = vec![l("DWG NO", 0.7, 0.9, 0.05), l("PID-0101", 0.7, 0.915, 0.06)];
        let refs: Vec<(u32, &Line)> = lines.iter().map(|x| (1, x)).collect();
        assert_eq!(d().metadata("x.pdf", &refs).drawing_no.as_deref(), Some("PID-0101"));
        let m = d().metadata("PID-0101 rev C.pdf", &[]);
        assert_eq!(m.drawing_no.as_deref(), Some("PID-0101"));
        assert_eq!(m.revision.as_deref(), Some("C"));
        assert!(m.evidence.iter().filter(|(f, _)| f == "drawing_number" || f == "revision").all(|(_, e)| e.contains("file name")));
    }

    #[test]
    fn bad_rule_is_reported_not_fatal() {
        let mut s = Settings::default();
        let mut rules = sleuth_core::settings::default_tag_rules();
        rules.push(sleuth_core::settings::TagRule { category: "bad".into(), pattern: "([".into(), enabled: true });
        s.tag_rules = Some(rules);
        let d = Deriver::new(&s);
        assert_eq!(d.errors.len(), 1);
        assert_eq!(d.tags("PSV-1203").len(), 1);
    }
}
