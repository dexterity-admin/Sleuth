//! ASCII DXF: pairs of lines, a group code and its value.
//!
//! Taken as lines: TEXT, MTEXT (formatting codes removed), ATTRIB values,
//! ATTDEF defaults (the tag where there is no default), dimension text
//! overrides, multileader text and table cell text. Text in model space is
//! placed against the drawing extents (`$EXTMIN` / `$EXTMAX`, or the union
//! of the text points when the header has none); text in paper space and in
//! block definitions has no place, because its coordinates are another
//! space's.
//!
//! Binary DXF is refused.

use crate::Out;
use crate::cad::{Extents, mtext_lines, plain_text};
use anyhow::{Context, Result};
use encoding_rs::Encoding;
use std::path::Path;

const BINARY_SENTINEL: &[u8] = b"AutoCAD Binary DXF";

/// Whether a drawing file is DXF rather than DWG: by its first bytes, and
/// by its extension only when those do not say.
pub fn looks_like_dxf(path: &Path, ext: &str) -> Result<bool> {
    let head = crate::head(path, 64).with_context(|| "cannot open the file")?;
    if head.starts_with(BINARY_SENTINEL) {
        return Ok(true);
    }
    // DWG begins with its version: AC1032, AC1.50, MC0.0 …
    if head.starts_with(b"AC") || head.starts_with(b"MC0") {
        return Ok(false);
    }
    let start = head.trim_ascii_start();
    let first_group = start.starts_with(b"0\r\nSECTION") || start.starts_with(b"0\nSECTION") || start.starts_with(b"999");
    Ok(first_group || ext == "dxf")
}

/// One entity's groups, as far as text needs them.
#[derive(Default)]
struct Entity {
    kind: String,
    /// Code 1 values in order (a table has one per cell).
    text: Vec<String>,
    /// Code 3: the leading 250-character chunks of long MTEXT.
    chunks: String,
    tag: String,
    /// Code 304 (multileader) and 302 (table cell) text.
    extra: Vec<String>,
    p10: (Option<f64>, Option<f64>),
    p11: (Option<f64>, Option<f64>),
    paper: bool,
    /// Past `101 Embedded Object`: the groups describe something else.
    embedded: bool,
}

struct Found {
    text: String,
    at: Option<(f64, f64)>,
}

fn pair(p: (Option<f64>, Option<f64>)) -> Option<(f64, f64)> {
    Some((p.0?, p.1?))
}

impl Entity {
    /// The lines this entity shows, and where it sits in its space.
    fn lines(&self) -> (Vec<String>, Option<(f64, f64)>) {
        let first = self.text.first().map(String::as_str).unwrap_or_default();
        match self.kind.as_str() {
            "TEXT" | "ATTRIB" => (vec![plain_text(first)], pair(self.p10)),
            "ATTDEF" => (vec![plain_text(if first.trim().is_empty() { &self.tag } else { first })], pair(self.p10)),
            "MTEXT" => (mtext_lines(&format!("{}{first}", self.chunks)), pair(self.p10)),
            // `<>` stands for the measured value, which is not text of the file.
            "DIMENSION" => (mtext_lines(&first.replace("<>", " ")), pair(self.p11)),
            "MULTILEADER" | "MLEADER" => (self.extra.iter().flat_map(|t| mtext_lines(t)).collect(), None),
            "ACAD_TABLE" => (self.text.iter().chain(self.extra.iter()).flat_map(|t| mtext_lines(t)).collect(), None),
            _ => (Vec::new(), None),
        }
    }
}

/// `ANSI_1252` → Windows-1252. `None` for a name we do not know.
fn dwg_code_page(name: &str) -> Option<&'static Encoding> {
    let n = name.trim().to_ascii_uppercase();
    let label = match n.strip_prefix("ANSI_").or_else(|| n.strip_prefix("DOS")) {
        Some("1250") => "windows-1250",
        Some("1251") => "windows-1251",
        Some("1252") => "windows-1252",
        Some("1253") => "windows-1253",
        Some("1254") => "windows-1254",
        Some("1255") => "windows-1255",
        Some("1256") => "windows-1256",
        Some("1257") => "windows-1257",
        Some("1258") => "windows-1258",
        Some("874") => "windows-874",
        Some("932") => "shift_jis",
        Some("936") => "gbk",
        Some("949") => "euc-kr",
        Some("950") => "big5",
        _ => return None,
    };
    Encoding::for_label(label.as_bytes())
}

/// A value's bytes as text: UTF-8 when valid (DXF since AutoCAD 2007, and
/// plain ASCII), otherwise the drawing's code page.
fn decode(v: &[u8], cp: Option<&'static Encoding>) -> String {
    match std::str::from_utf8(v) {
        Ok(s) => s.to_string(),
        Err(_) => cp.unwrap_or(encoding_rs::WINDOWS_1252).decode_without_bom_handling(v).0.into_owned(),
    }
}

fn number(v: &[u8]) -> Option<f64> {
    std::str::from_utf8(v).ok()?.trim().parse().ok().filter(|f: &f64| f.is_finite())
}

pub(crate) fn read_bytes(b: &[u8], out: &mut Out, limit: usize) -> Result<()> {
    if b.starts_with(BINARY_SENTINEL) {
        anyhow::bail!("binary DXF is not supported");
    }
    let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b);
    let mut lines = b.split(|c| *c == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let mut found: Vec<Found> = Vec::new();
    let mut union = Extents::empty();
    let (mut ext_min, mut ext_max) = ((None, None), (None, None));
    let mut cp: Option<&'static Encoding> = None;
    let mut section = String::new();
    let mut variable = String::new();
    let mut expect_section_name = false;
    let mut entity: Option<Entity> = None;
    let mut groups = 0usize;
    let mut line_no = 0usize;
    let mut damaged: Option<String> = None;
    let mut saw_eof = false;

    let finish = |e: Option<Entity>, section: &str, found: &mut Vec<Found>, union: &mut Extents| {
        let Some(e) = e else { return };
        let (texts, at) = e.lines();
        // Only model space shares the extents' coordinates.
        let at = at.filter(|_| section == "ENTITIES" && !e.paper);
        for text in texts {
            if text.trim().is_empty() || found.len() >= limit {
                continue;
            }
            if let Some(p) = at {
                union.add(p);
            }
            found.push(Found { text, at });
        }
    };

    loop {
        out.tick()?;
        let Some(code_line) = lines.next() else { break };
        line_no += 1;
        let code_text = String::from_utf8_lossy(code_line);
        let code_text = code_text.trim();
        if code_text.is_empty() {
            continue; // blank lines between groups, and the end of the file
        }
        let Ok(code) = code_text.parse::<i32>() else {
            damaged = Some(format!("line {line_no} is not a group code"));
            break;
        };
        let Some(value) = lines.next() else {
            damaged = Some("the file ends in the middle of a group".into());
            break;
        };
        line_no += 1;
        groups += 1;
        if code == 0 {
            finish(entity.take(), &section, &mut found, &mut union);
            let name = decode(value, None).trim().to_ascii_uppercase();
            match name.as_str() {
                "SECTION" => expect_section_name = true,
                "ENDSEC" => section.clear(),
                "EOF" => {
                    saw_eof = true;
                    break;
                }
                _ if section == "ENTITIES" || section == "BLOCKS" => entity = Some(Entity { kind: name, ..Default::default() }),
                _ => {}
            }
            if found.len() >= limit {
                break;
            }
            continue;
        }
        if expect_section_name && code == 2 {
            section = decode(value, None).trim().to_ascii_uppercase();
            expect_section_name = false;
            continue;
        }
        if section == "HEADER" {
            match code {
                9 => variable = decode(value, None).trim().to_ascii_uppercase(),
                3 if variable == "$DWGCODEPAGE" => cp = dwg_code_page(&decode(value, None)),
                10 if variable == "$EXTMIN" => ext_min.0 = number(value),
                20 if variable == "$EXTMIN" => ext_min.1 = number(value),
                10 if variable == "$EXTMAX" => ext_max.0 = number(value),
                20 if variable == "$EXTMAX" => ext_max.1 = number(value),
                _ => {}
            }
            continue;
        }
        let Some(e) = entity.as_mut() else { continue };
        if e.embedded {
            continue;
        }
        match code {
            1 => e.text.push(decode(value, cp)),
            // In an ATTDEF code 3 is the prompt, not text of the drawing.
            3 if e.kind == "MTEXT" => e.chunks.push_str(&decode(value, cp)),
            2 if e.kind == "ATTDEF" || e.kind == "ATTRIB" => e.tag = decode(value, cp),
            302 | 304 => e.extra.push(decode(value, cp)),
            10 if e.p10.0.is_none() => e.p10.0 = number(value),
            20 if e.p10.1.is_none() => e.p10.1 = number(value),
            11 if e.p11.0.is_none() => e.p11.0 = number(value),
            21 if e.p11.1.is_none() => e.p11.1 = number(value),
            67 => e.paper = number(value) == Some(1.0),
            101 => e.embedded = true,
            _ => {}
        }
    }
    finish(entity.take(), &section, &mut found, &mut union);
    if groups == 0 || (found.is_empty() && damaged.is_some()) {
        anyhow::bail!("damaged or not a DXF file{}", damaged.map(|d| format!(": {d}")).unwrap_or_default());
    }

    let extents = pair(ext_min).zip(pair(ext_max)).and_then(|(lo, hi)| Extents::stated(lo, hi)).unwrap_or(union);
    out.page("model", "");
    for f in &found {
        let bbox = f.at.and_then(|p| extents.point(p)).unwrap_or_default();
        if !out.line_at(&f.text, bbox) {
            break;
        }
    }
    if let Some(d) = damaged {
        out.note(&format!("the file is damaged ({d}); text after that point is missing"));
    } else if !saw_eof && found.len() < limit {
        out.note("the file has no end marker; it may be cut short");
    }
    Ok(())
}

pub fn read(path: &Path, out: &mut Out) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| "cannot read the file")?;
    let limit = out.room().saturating_add(1);
    read_bytes(&bytes, out, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_pages() {
        assert_eq!(dwg_code_page("ANSI_1252").map(|e| e.name()), Some("windows-1252"));
        assert_eq!(dwg_code_page("ansi_932").map(|e| e.name()), Some("Shift_JIS"));
        assert!(dwg_code_page("UNDEFINED").is_none());
        assert_eq!(decode(b"\xD850", dwg_code_page("ANSI_1252")), "\u{d8}50");
        assert_eq!(decode("Ø50".as_bytes(), dwg_code_page("ANSI_1252")), "\u{d8}50");
    }
}
