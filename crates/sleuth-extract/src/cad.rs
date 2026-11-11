//! What AutoCAD text looks like on the drawing rather than in the file:
//! MTEXT formatting codes removed, `%%c` style codes resolved; and where a
//! text sits, normalised to the drawing extents.

use sleuth_core::protocol::NormBox;

/// `\U+00B0` → `°`. Returns the character and the bytes used.
fn unicode_escape(s: &str) -> Option<(char, usize)> {
    let hex = s.strip_prefix("\\U+").or_else(|| s.strip_prefix("\\u+"))?;
    let digits = hex.get(..4).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))?;
    char::from_u32(u32::from_str_radix(digits, 16).ok()?).map(|c| (c, 7))
}

/// `%%c`, `%%d`, `%%p`, `%%nnn` and the `%%u` / `%%o` / `%%k` toggles, as
/// used by TEXT, ATTRIB and MTEXT alike. Returns the text and the bytes used.
fn percent_code(s: &str) -> Option<(&'static str, Option<char>, usize)> {
    let rest = s.strip_prefix("%%")?;
    let c = rest.chars().next()?;
    Some(match c.to_ascii_lowercase() {
        'c' => ("\u{d8}", None, 3),
        'd' => ("\u{b0}", None, 3),
        'p' => ("\u{b1}", None, 3),
        'u' | 'o' | 'k' => ("", None, 3),
        '%' => ("%", None, 3),
        '0'..='9' => {
            let digits = rest.get(..3).filter(|d| d.bytes().all(|b| b.is_ascii_digit()))?;
            let code: u32 = digits.parse().ok()?;
            // Codes are positions in the drawing's 8-bit font.
            let ch = encoding_rs::WINDOWS_1252.decode_without_bom_handling(&[u8::try_from(code).ok()?]).0.chars().next()?;
            ("", Some(ch), 5)
        }
        _ => return None,
    })
}

/// Single-line text (TEXT, ATTRIB, ATTDEF, dimension overrides) as shown.
pub(crate) fn plain_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if let Some((ch, n)) = unicode_escape(rest) {
            out.push(ch);
            rest = &rest[n..];
        } else if let Some((text, ch, n)) = percent_code(rest) {
            out.push_str(text);
            out.extend(ch);
            rest = &rest[n..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// MTEXT content as the lines a reader sees: formatting codes go, `\P`
/// starts a new line, stacked text (`\S1/2;`) becomes `1/2`.
pub(crate) fn mtext_lines(s: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '\\' {
            if let Some((ch, n)) = unicode_escape(rest) {
                cur.push(ch);
                rest = &rest[n..];
                continue;
            }
            let Some(code) = rest[1..].chars().next() else { break };
            let after = &rest[1 + code.len_utf8()..];
            // Codes that carry a value run to the next semicolon.
            let value_end = || after.find(';').map(|i| (&after[..i], &after[i + 1..]));
            match code {
                'P' | 'X' | 'N' => {
                    lines.push(std::mem::take(&mut cur));
                    rest = after;
                }
                '~' => {
                    cur.push(' ');
                    rest = after;
                }
                '\\' | '{' | '}' => {
                    cur.push(code);
                    rest = after;
                }
                'L' | 'l' | 'O' | 'o' | 'K' | 'k' => rest = after,
                'S' => match value_end() {
                    Some((stack, tail)) => {
                        // Numerator and denominator, split by `/`, `#` or `^`.
                        let mut parts = stack.splitn(2, ['/', '#', '^']);
                        let top = parts.next().unwrap_or_default().trim();
                        let bottom = parts.next().map(str::trim).unwrap_or_default();
                        cur.push_str(top);
                        if !top.is_empty() && !bottom.is_empty() {
                            cur.push(if stack.contains('^') { ' ' } else { '/' });
                        }
                        cur.push_str(bottom);
                        rest = tail;
                    }
                    None => rest = after,
                },
                'f' | 'F' | 'H' | 'h' | 'W' | 'w' | 'Q' | 'q' | 'T' | 't' | 'A' | 'a' | 'C' | 'c' | 'p' => match value_end() {
                    Some((_, tail)) => rest = tail,
                    None => rest = after,
                },
                // `\M+nXXXX` is a character of a multi-byte code page that is
                // not known here.
                'M' | 'm' if after.starts_with('+') && after.len() >= 6 && after.is_char_boundary(6) => {
                    cur.push('\u{fffd}');
                    rest = &after[6..];
                }
                other => {
                    // Not a code: the text holds a backslash.
                    cur.push('\\');
                    cur.push(other);
                    rest = after;
                }
            }
        } else if c == '{' || c == '}' {
            rest = &rest[1..];
        } else if c == '^' && rest.len() >= 2 && matches!(rest.as_bytes()[1], b'I' | b'J' | b'M') {
            // Caret codes: tab, line feed, carriage return.
            if rest.as_bytes()[1] == b'I' { cur.push(' ') } else { lines.push(std::mem::take(&mut cur)) }
            rest = &rest[2..];
        } else if let Some((text, ch, n)) = percent_code(rest) {
            cur.push_str(text);
            cur.extend(ch);
            rest = &rest[n..];
        } else if c == '\n' || c == '\r' {
            lines.push(std::mem::take(&mut cur));
            rest = &rest[1..];
        } else {
            cur.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    lines.push(cur);
    lines.into_iter().map(|l| crate::out::clean(&l)).filter(|l| !l.is_empty()).collect()
}

/// The rectangle text positions are measured against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Extents {
    pub min: (f64, f64),
    pub max: (f64, f64),
}

impl Extents {
    /// Nothing seen yet.
    pub(crate) fn empty() -> Extents {
        Extents { min: (f64::INFINITY, f64::INFINITY), max: (f64::NEG_INFINITY, f64::NEG_INFINITY) }
    }

    /// Extents a file states are usable when they are finite and not the
    /// "nothing drawn" placeholder (±1e20).
    pub(crate) fn stated(min: (f64, f64), max: (f64, f64)) -> Option<Extents> {
        let ok = [min.0, min.1, max.0, max.1].iter().all(|v| v.is_finite() && v.abs() < 1e19) && max.0 > min.0 && max.1 > min.1;
        ok.then_some(Extents { min, max })
    }

    pub(crate) fn add(&mut self, p: (f64, f64)) {
        if p.0.is_finite() && p.1.is_finite() {
            self.min = (self.min.0.min(p.0), self.min.1.min(p.1));
            self.max = (self.max.0.max(p.0), self.max.1.max(p.1));
        }
    }

    pub(crate) fn valid(&self) -> bool {
        self.max.0 > self.min.0 && self.max.1 > self.min.1
    }

    /// A rectangle in drawing units as a page box: 0..1, y down. `None`
    /// when it lies outside the extents (another space, a block definition).
    pub(crate) fn norm(&self, lo: (f64, f64), hi: (f64, f64)) -> Option<NormBox> {
        if !self.valid() || ![lo.0, lo.1, hi.0, hi.1].iter().all(|v| v.is_finite()) {
            return None;
        }
        let (w, h) = (self.max.0 - self.min.0, self.max.1 - self.min.1);
        let x0 = (lo.0 - self.min.0) / w;
        let x1 = (hi.0 - self.min.0) / w;
        let y0 = 1.0 - (hi.1 - self.min.1) / h;
        let y1 = 1.0 - (lo.1 - self.min.1) / h;
        // A little slack: extents are often stale by a text height.
        let inside = |v: f64| (-0.02..=1.02).contains(&v);
        if !(inside(x0) && inside(x1) && inside(y0) && inside(y1)) {
            return None;
        }
        let c = |v: f64| v.clamp(0.0, 1.0) as f32;
        Some(NormBox { x: c(x0), y: c(y0), w: (c(x1) - c(x0)).max(0.0), h: (c(y1) - c(y0)).max(0.0) })
    }

    pub(crate) fn point(&self, p: (f64, f64)) -> Option<NormBox> {
        self.norm(p, p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtext_codes_are_stripped() {
        let s = "{\\fArial|b0|i0|c0|p34;GENERAL NOTES}\\P1. ALL DIMENSIONS IN MM\\P2. LINE 6\"-P-1001-A1A \\A1;\\H2.5x;TO PUMP P-101A";
        assert_eq!(mtext_lines(s), ["GENERAL NOTES", "1. ALL DIMENSIONS IN MM", "2. LINE 6\"-P-1001-A1A TO PUMP P-101A"]);
        assert_eq!(mtext_lines("\\pi-3,l3,t3;\\LUNDERLINED\\l and \\C1;red\\~text"), ["UNDERLINED and red text"]);
        assert_eq!(mtext_lines("DN\\S1/2;  TOL \\S+0.1^-0.2; \\S3#4;"), ["DN1/2 TOL +0.1 -0.2 3/4"]);
        assert_eq!(mtext_lines("PATH C:\\\\TEMP \\{kept\\} %%c50 45%%d \\U+00B11"), ["PATH C:\\TEMP {kept} \u{d8}50 45\u{b0} \u{b1}1"]);
        assert_eq!(mtext_lines("A^JB^IC"), ["A", "B C"]);
        assert_eq!(mtext_lines("{\\fArial;}\\P"), Vec::<String>::new());
        assert_eq!(mtext_lines("ends with a backslash\\"), ["ends with a backslash"]);
    }

    #[test]
    fn text_codes() {
        assert_eq!(plain_text("PSV-1203 %%C50 45%%d %%uUNDER%%u %%177 100%%%"), "PSV-1203 \u{d8}50 45\u{b0} UNDER \u{b1} 100%");
        assert_eq!(plain_text("\\U+00D850 %%x 50%"), "\u{d8}50 %%x 50%");
    }

    #[test]
    fn positions_normalise() {
        let e = Extents::stated((0.0, 0.0), (400.0, 200.0)).unwrap();
        let b = e.point((100.0, 150.0)).unwrap();
        assert!((b.x - 0.25).abs() < 1e-6 && (b.y - 0.25).abs() < 1e-6 && b.w == 0.0 && b.h == 0.0);
        let b = e.norm((0.0, 0.0), (200.0, 100.0)).unwrap();
        assert!((b.y - 0.5).abs() < 1e-6 && (b.w - 0.5).abs() < 1e-6 && (b.h - 0.5).abs() < 1e-6);
        assert!(e.point((5000.0, 10.0)).is_none(), "outside the extents: no place claimed");
        assert!(Extents::stated((1e20, 1e20), (-1e20, -1e20)).is_none());
    }
}
