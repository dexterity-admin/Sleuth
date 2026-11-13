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
/// 3: a title-block value must fill its whole cell, a label must start its
/// cell, a REV column of a revision table is not a revision, and status
/// words count only as whole words on a line of their own.
pub const PARSER_VERSION: u32 = 3;

/// The part of the title-block rules' identity that goes into the OCR
/// pipeline fingerprint. Kept at the value 1.1 used, so PCs on different
/// versions keep sharing readings of the same content.
const TITLE_OCR_VERSION: u32 = 2;

/// Column headings of revision tables and title blocks. Such a cell is
/// never the value of a field, even when it is short enough to look like one.
const HEADINGS: &[&str] = &[
    "DRN", "DRAWN", "DRWN", "CHK", "CHKD", "CHECKED", "CKD", "APP", "APPD", "APPR", "APPROVED", "BY", "DATE", "DESC", "DESCRIPTION", "REMARKS", "SCALE", "NTS", "ISSUE",
    "ISSUED", "REVISION", "REV", "NO", "SHEET", "TITLE", "STATUS", "PURPOSE",
];

/// Trailing punctuation a cell may carry after its value.
const TAIL: &[char] = &['.', ',', ';', ':', ')', ']', '}', '*'];

/// The current rules, for code that shows stored title-block details
/// (`Store::metadata`): set by the engine whenever the rules change.
static CURRENT: std::sync::RwLock<Option<std::sync::Arc<Deriver>>> = std::sync::RwLock::new(None);

pub fn set_current(d: std::sync::Arc<Deriver>) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(d);
}

/// The rules in use (the default rules until the engine sets them).
pub fn current() -> std::sync::Arc<Deriver> {
    if let Some(d) = CURRENT.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return std::sync::Arc::clone(d);
    }
    let d = std::sync::Arc::new(Deriver::new(&Settings::default()));
    let mut g = CURRENT.write().unwrap_or_else(|e| e.into_inner());
    std::sync::Arc::clone(g.get_or_insert(d))
}

fn alnum_len(s: &str) -> usize {
    s.chars().filter(|c| c.is_alphanumeric()).count()
}

/// A cell that is only a column heading such as DR'N, CHK'D or DATE.
fn is_heading(text: &str) -> bool {
    let a: String = text.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_uppercase();
    !a.is_empty() && HEADINGS.contains(&a.as_str())
}

/// `word` in `text` as a whole word (not inside a longer word: VOID is not in AVOID).
fn whole_word(text: &str, word: &str) -> bool {
    let up = text.to_uppercase();
    let mut from = 0;
    while let Some(i) = up[from..].find(word) {
        let s = from + i;
        let e = s + word.len();
        let before = up[..s].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = up[e..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        if before && after {
            return true;
        }
        from = s + word.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// The quoted text an evidence string was taken from: `page 1: "REV. A"`,
/// `page 1: "A", below the label "REV"`, `page 1: "A1", OCR 93%`. A quote
/// inside the text (an inch mark) is not taken for its end.
fn evidence_text(evidence: &str) -> Option<&str> {
    let rest = evidence.strip_prefix("page ")?;
    let open = rest.find(": \"")? + 3;
    let body = &rest[open..];
    let ends = [", OCR ", ", checked by", ", below the label", ", right of the label"];
    body.char_indices().filter(|(_, c)| *c == '"').map(|(i, _)| i).find(|&i| {
        let after = &body[i + 1..];
        after.is_empty() || ends.iter().any(|e| after.starts_with(e))
    }).map(|i| &body[..i])
}

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
        let fields_hash = blake3::hash(format!("{TITLE_OCR_VERSION}{}", serde_json::to_string(&frules).unwrap_or_default()).as_bytes()).to_hex()[..16].to_string();
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
            // Longest first: ISSUED FOR CONSTRUCTION before FOR CONSTRUCTION.
            let mut words: Vec<&str> = STATUS_WORDS.to_vec();
            words.sort_by_key(|w| std::cmp::Reverse(w.len()));
            'status: for (_, l) in &first {
                for w in &words {
                    if status_line(&l.text, w) {
                        m.status = Some(w.to_string());
                        m.evidence.push(("status".into(), format!("page 1: \"{}\"", l.text.trim())));
                        break 'status;
                    }
                }
            }
        }
        m
    }

    /// Take out of stored title-block details every value its own evidence
    /// does not support under the current rules (a value cut out of a longer
    /// cell, a column heading, a status word inside another word). Readings
    /// made with older rules then show "not found" instead of a wrong value;
    /// nothing is ever added here.
    pub fn recheck(&self, m: &mut DrawingMeta) {
        let mut wrong: Vec<String> = Vec::new();
        for (field, ev) in &m.evidence {
            let Some(text) = evidence_text(ev) else { continue };
            let value = match field.as_str() {
                "drawing_number" => m.drawing_no.as_deref(),
                "revision" => m.revision.as_deref(),
                "title" => m.title.as_deref(),
                "project" => m.project.as_deref(),
                "sheet" => m.sheet.as_deref(),
                "discipline" => m.discipline.as_deref(),
                "status" => m.status.as_deref(),
                _ => None,
            };
            let Some(value) = value else { continue };
            let Some(f) = self.fields.iter().find(|f| f.name == *field) else { continue };
            let ok = if field == "status" && self.label_rest(f, text).is_none() {
                STATUS_WORDS.iter().any(|w| value.eq_ignore_ascii_case(w) && status_line(text, w))
            } else {
                self.supports(f, text, value)
            };
            if !ok {
                wrong.push(field.clone());
            }
        }
        if wrong.iter().any(|f| f == "drawing_number") && m.evidence.iter().any(|(f, e)| f == "discipline" && e.starts_with("drawing number")) {
            wrong.push("discipline".into());
        }
        for f in &wrong {
            match f.as_str() {
                "drawing_number" => m.drawing_no = None,
                "revision" => m.revision = None,
                "title" => m.title = None,
                "project" => m.project = None,
                "sheet" => m.sheet = None,
                "discipline" => m.discipline = None,
                "status" => m.status = None,
                _ => {}
            }
        }
        m.evidence.retain(|(f, _)| !wrong.contains(f));
    }

    /// Whether the line `text` gives `value` for field `f`: as the rest of a
    /// cell that starts with the field's label, or as a cell of its own.
    fn supports(&self, f: &Field, text: &str, value: &str) -> bool {
        let got = match self.label_rest(f, text) {
            Some(rest) => self.cell_value(f, rest, true),
            None => (!self.is_label(text)).then(|| self.cell_value(f, text, false)).flatten(),
        };
        got.is_some_and(|v| v.eq_ignore_ascii_case(value.trim()))
    }

    /// The text after a label of `f` that starts the cell `text` (only
    /// punctuation before it). `None` when no label starts it: "CLIENT
    /// DRAWING NO" is not a DRAWING NO cell, "NOTE ... REV B" is no REV cell.
    fn label_rest<'t>(&self, f: &Field, text: &'t str) -> Option<&'t str> {
        let text = text.trim();
        let rest = |end: usize| text[end..].trim_start_matches(|c: char| c == ':' || c == '.' || c == '#' || c == '-' || c.is_whitespace());
        f.labels.iter().find_map(|label| {
            label.find_iter(text).find_map(|m| {
                let before = &text[..m.start()];
                // The label starts the cell, or follows a whole cell of another
                // field on the same text line ("DRAWING NO. ISO-1001-01 REV 2").
                let starts = before.chars().all(|c| !c.is_alphanumeric());
                let after_cell = !starts && self.fields.iter().filter(|g| g.name != f.name).any(|g| self.label_rest(g, before).is_some_and(|r| self.cell_value(g, r, false).is_some()));
                (starts || after_cell).then(|| rest(m.end()))
            })
        })
    }

    /// The value of `f` that `cell` holds, if the cell holds nothing else:
    /// only punctuation may follow it, or (when `more` is allowed: the rest
    /// of a line after a label) the next cell's label or heading.
    fn cell_value(&self, f: &Field, cell: &str, more: bool) -> Option<String> {
        let cell = cell.trim();
        let m = f.value.find(cell)?;
        let v = m.as_str().trim().trim_end_matches([',', ';']).trim();
        if v.is_empty() || is_heading(v) || (f.name == "drawing_number" && !v.chars().any(|c| c.is_ascii_digit())) {
            return None;
        }
        let after = cell[m.end()..].trim();
        let after_bare = after.trim_matches(|c: char| TAIL.contains(&c) || c.is_whitespace());
        let ends_here = after_bare.is_empty();
        let next_cell = more && (self.starts_with_label(after) || after.split_whitespace().next().is_some_and(is_heading));
        (ends_here || next_cell).then(|| v.to_string())
    }

    /// Text that starts with a label of any field.
    fn starts_with_label(&self, text: &str) -> bool {
        self.fields.iter().any(|f| self.label_rest(f, text).is_some())
    }
}

/// A line that states a status: the word stands on its own (VOID is not in
/// AVOID) and the line is little more than it (a stamp or a title-block
/// cell, not a sentence of a note).
fn status_line(text: &str, word: &str) -> bool {
    whole_word(text, word) && alnum_len(text) <= alnum_len(word) + 12
}

impl Deriver {
    /// Find one field: a cell that starts with its label, then its value as
    /// the rest of that cell, or as the whole of the cell just right of it,
    /// or just below it. Lines in the title-block area are tried first.
    /// Anything less certain gives no value: a blank is checked by a person,
    /// a wrong value is believed.
    fn field_value(&self, name: &str, lines: &[(u32, &Line)]) -> Option<(String, String)> {
        let f = self.fields.iter().find(|f| f.name == name)?;
        let mut order: Vec<&(u32, &Line)> = lines.iter().collect();
        order.sort_by_key(|(_, l)| if TITLE_BLOCK.overlap(&l.bbox) > 0.5 { 0 } else { 1 });
        for (page, l) in order {
            let text = l.text.trim();
            let Some(rest) = self.label_rest(f, text) else { continue };
            let how = |from: &Line| match from.source {
                TextSource::Ocr => format!(", OCR {}%", (from.conf * 100.0).round()),
                TextSource::Verified => ", checked by a person".into(),
                _ => String::new(),
            };
            if let Some(v) = self.cell_value(f, rest, true) {
                return Some((v, format!("page {page}: \"{text}\"{}", how(l))));
            }
            // The label is followed by other words ("SHEET 1 TO 6"): a note, not a cell.
            if !rest.is_empty() && !self.starts_with_label(rest) && !rest.split_whitespace().next().is_some_and(is_heading) {
                continue;
            }
            let Some((n, place)) = self.neighbour(f, l, lines) else { continue };
            let Some(v) = self.cell_value(f, &n.text, false) else { continue };
            return Some((v, format!("page {page}: \"{}\", {place} the label \"{text}\"{}", n.text.trim(), how(n))));
        }
        None
    }

    /// Text that holds a label of any field (so it is not a value).
    fn is_label(&self, text: &str) -> bool {
        self.fields.iter().any(|f| f.labels.iter().any(|r| r.is_match(text)))
    }

    /// The cell next to a label cell: the nearest one to the right on the
    /// same row, else the nearest one below in the same column. It must be
    /// a value of its own; a nearer cell is never skipped for a farther one.
    /// For a revision, a value with more values under it is a column of a
    /// revision table, where the current revision is not certain: none.
    fn neighbour<'a>(&self, f: &Field, l: &Line, lines: &'a [(u32, &'a Line)]) -> Option<(&'a Line, &'static str)> {
        let cy = l.bbox.y + l.bbox.h / 2.0;
        let h = l.bbox.h.max(0.004);
        let is_value = |n: &Line| !self.is_label(&n.text) && self.cell_value(f, &n.text, false).is_some();
        let right = lines
            .iter()
            .map(|(_, n)| *n)
            .filter(|n| !std::ptr::eq(*n, l))
            .filter(|n| ((n.bbox.y + n.bbox.h / 2.0) - cy).abs() < h * 0.6 && n.bbox.x > l.bbox.x + l.bbox.w - h && n.bbox.x - (l.bbox.x + l.bbox.w) < h * 12.0)
            .min_by(|a, b| a.bbox.x.total_cmp(&b.bbox.x));
        if let Some(r) = right {
            return is_value(r).then_some((r, "right of"));
        }
        let column = |top: &Line| {
            let th = top.bbox.h.max(0.004);
            lines
                .iter()
                .map(|(_, n)| *n)
                .filter(move |n| n.bbox.y > top.bbox.y + th * 0.5 && n.bbox.y - (top.bbox.y + top.bbox.h) < th * 2.5)
                .filter(move |n| n.bbox.x < l.bbox.x + l.bbox.w + h * 4.0 && n.bbox.x + n.bbox.w > l.bbox.x - h * 2.0)
                .min_by(|a, b| a.bbox.y.total_cmp(&b.bbox.y))
        };
        let below = column(l)?;
        if !is_value(below) {
            return None;
        }
        if f.name == "revision" && column(below).is_some_and(|n| is_value(n)) {
            return None;
        }
        Some((below, "below"))
    }
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

    /// What was found wrong on a real isometric (12 Nov 2026): Revision "DR"
    /// from the DR'N. heading of the revision table, Status "VOID" from
    /// "AVOID" in a note, Sheet "1" out of "(SHEET 1 TO 6)".
    #[test]
    fn values_cut_out_of_other_cells_are_not_taken() {
        let lines = vec![
            l("REV", 0.60, 0.80, 0.02),
            l("DR'N.", 0.63, 0.80, 0.03),
            l("CHK'D", 0.67, 0.80, 0.03),
            l("HCV IS SIZED TO AVOID CLOSED DRAIN OVERLOADING AND LOCATED AS CLOSE AS", 0.05, 0.30, 0.4),
            l("(SHEET 1 TO 6)", 0.20, 0.40, 0.06),
            l("PROJECT No. P12513", 0.70, 0.95, 0.1),
        ];
        let refs: Vec<(u32, &Line)> = lines.iter().map(|x| (1, x)).collect();
        let m = d().metadata("P12513-12-99-08-1608.pdf", &refs);
        assert_eq!(m.revision, None, "{:?}", m.evidence);
        assert_eq!(m.status, None, "{:?}", m.evidence);
        assert_eq!(m.sheet, None, "{:?}", m.evidence);
        assert_eq!(m.project.as_deref(), Some("P12513"));
    }

    #[test]
    fn a_revision_table_column_gives_no_revision() {
        let table = vec![l("REV", 0.60, 0.80, 0.02), l("0", 0.60, 0.815, 0.01), l("A", 0.60, 0.83, 0.01)];
        let refs: Vec<(u32, &Line)> = table.iter().map(|x| (1, x)).collect();
        assert_eq!(d().metadata("x.pdf", &refs).revision, None);
        // One cell under its label is the title block's revision.
        let cell = vec![l("REV", 0.90, 0.95, 0.02), l("B", 0.90, 0.965, 0.01)];
        let refs: Vec<(u32, &Line)> = cell.iter().map(|x| (1, x)).collect();
        let m = d().metadata("x.pdf", &refs);
        assert_eq!(m.revision.as_deref(), Some("B"));
        assert!(m.evidence.iter().any(|(f, e)| f == "revision" && e == "page 1: \"B\", below the label \"REV\""), "{:?}", m.evidence);
        // A value followed by the next cell's label on the same line.
        let row = vec![l("DRAWING NO. ISO-1001-01 REV 2", 0.6, 0.9, 0.3)];
        let refs: Vec<(u32, &Line)> = row.iter().map(|x| (1, x)).collect();
        let m = d().metadata("x.pdf", &refs);
        assert_eq!(m.drawing_no.as_deref(), Some("ISO-1001-01"));
        assert_eq!(m.revision.as_deref(), Some("2"));
    }

    #[test]
    fn stored_values_their_evidence_does_not_support_are_taken_out() {
        let ev = |f: &str, e: &str| (f.to_string(), e.to_string());
        let mut m = DrawingMeta {
            drawing_no: Some("P12513-12-99-08-1608".into()),
            revision: Some("DR".into()),
            project: Some("P12513".into()),
            sheet: Some("1".into()),
            status: Some("VOID".into()),
            evidence: vec![
                ev("drawing_number", "page 1: \"P12513-12-99-08-1608\""),
                ev("revision", "page 1: \"DR'N.\""),
                ev("project", "page 1: \"PROJECT No. P12513\""),
                ev("sheet", "page 1: \"(SHEET 1 TO 6)\""),
                ev("status", "page 1: \"HCV IS SIZED TO AVOID CLOSED DRAIN OVERLOADING AND LOCATED AS CLOSE AS\""),
            ],
            ..Default::default()
        };
        d().recheck(&mut m);
        assert_eq!(m.drawing_no.as_deref(), Some("P12513-12-99-08-1608"));
        assert_eq!(m.project.as_deref(), Some("P12513"));
        assert_eq!((m.revision, m.sheet, m.status), (None, None, None));
        assert_eq!(m.evidence.len(), 2);
        // Values found under the new rules pass unchanged, OCR notes and inch marks included.
        let mut ok = DrawingMeta { revision: Some("B".into()), title: Some("6\"-P-1001 TIE-IN".into()), ..Default::default() };
        ok.evidence = vec![ev("revision", "page 1: \"B\", below the label \"REV\", OCR 91%"), ev("title", "page 1: \"TITLE: 6\"-P-1001 TIE-IN\"")];
        let before = ok.clone();
        d().recheck(&mut ok);
        assert_eq!(ok, before);
    }

    #[test]
    fn status_words_are_whole_words() {
        assert!(whole_word("VOID", "VOID") && whole_word("- VOID -", "VOID"));
        assert!(!whole_word("AVOID", "VOID") && !whole_word("VOIDED", "VOID"));
        assert!(status_line("ISSUED FOR CONSTRUCTION", "ISSUED FOR CONSTRUCTION"));
        assert!(!status_line("THIS LINE WAS VOID UNTIL THE PUMP WAS MOVED TO AREA 2", "VOID"));
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
