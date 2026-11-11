//! Plain text: .txt .log as lines, .csv .tsv as rows of cells.
//!
//! The encoding is taken from the byte-order mark; without one the bytes are
//! UTF-8 when they are valid UTF-8, and Windows-1252 otherwise (the code page
//! of the tools that write these files here).

use crate::Out;
use anyhow::{Context, Result};
use encoding_rs::{UTF_16BE, UTF_16LE, WINDOWS_1252};
use std::borrow::Cow;
use std::path::Path;

/// Bytes to text. An error only for content that is not text at all.
pub fn decode(bytes: &[u8]) -> Result<Cow<'_, str>> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(rest));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Ok(UTF_16LE.decode_without_bom_handling(rest).0);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Ok(UTF_16BE.decode_without_bom_handling(rest).0);
    }
    // UTF-16 written without a mark shows as text with every other byte zero.
    let sample = &bytes[..bytes.len().min(1 << 16)];
    let zeros = sample.iter().filter(|b| **b == 0).count();
    if zeros > 0 {
        let pairs = (sample.len() / 2).max(1);
        let even = sample.iter().step_by(2).filter(|b| **b == 0).count();
        let odd = zeros - even;
        if odd * 10 >= pairs * 4 && even * 20 <= pairs {
            return Ok(UTF_16LE.decode_without_bom_handling(bytes).0);
        }
        if even * 10 >= pairs * 4 && odd * 20 <= pairs {
            return Ok(UTF_16BE.decode_without_bom_handling(bytes).0);
        }
        if zeros * 10 > sample.len() {
            anyhow::bail!("not a text file: the content is binary");
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(Cow::Borrowed(s)),
        Err(_) => Ok(WINDOWS_1252.decode_without_bom_handling(bytes).0),
    }
}

/// The separator of a delimited file, from its first line with text: the
/// one of `,` `;` tab `|` that occurs most outside quotes (a comma when none
/// does).
fn separator(text: &str) -> char {
    let Some(first) = text.lines().find(|l| !l.trim().is_empty()) else { return ',' };
    let mut counts = [(',', 0usize), (';', 0), ('\t', 0), ('|', 0)];
    let mut quoted = false;
    for c in first.chars() {
        if c == '"' {
            quoted = !quoted;
        } else if !quoted {
            for (sep, n) in counts.iter_mut() {
                if *sep == c {
                    *n += 1;
                }
            }
        }
    }
    let mut best = counts[0];
    for c in counts {
        if c.1 > best.1 {
            best = c;
        }
    }
    best.0
}

/// Rows of a delimited file as lines, cells joined with ` | `. Quoted cells
/// may hold the separator, doubled quotes and line breaks.
pub(crate) fn delimited(text: &str, sep: char, out: &mut Out) -> Result<()> {
    let mut cells: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let flush_row = |cells: &mut Vec<String>, out: &mut Out| -> Result<bool> {
        out.tick()?;
        let row = cells.iter().map(|c| crate::out::clean(c)).filter(|c| !c.is_empty()).collect::<Vec<_>>().join(" | ");
        cells.clear();
        Ok(out.line(&row))
    };
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
        } else if c == '"' && cell.trim().is_empty() {
            cell.clear();
            quoted = true;
        } else if c == sep {
            cells.push(std::mem::take(&mut cell));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            cells.push(std::mem::take(&mut cell));
            if !flush_row(&mut cells, out)? {
                return Ok(());
            }
        } else {
            cell.push(c);
        }
    }
    cells.push(cell);
    flush_row(&mut cells, out)?;
    Ok(())
}

/// Text already decoded, as one page.
pub(crate) fn lines(text: &str, ext: &str, out: &mut Out) -> Result<()> {
    match ext {
        "csv" => delimited(text, separator(text), out),
        "tsv" => delimited(text, '\t', out),
        _ => {
            for l in text.split(['\n', '\r']) {
                out.tick()?;
                if !out.line(l) {
                    break;
                }
            }
            Ok(())
        }
    }
}

pub fn read(path: &Path, ext: &str, out: &mut Out) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| "cannot read the file")?;
    let text = decode(&bytes)?;
    out.page("text", "");
    lines(&text, ext, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        let s = "PSV-1203 Ø50 – 45°";
        assert_eq!(decode(s.as_bytes()).unwrap(), s);
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(s.as_bytes());
        assert_eq!(decode(&bom).unwrap(), s);
        let le: Vec<u8> = [0xFF, 0xFE].into_iter().chain(s.encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(decode(&le).unwrap(), s);
        let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain(s.encode_utf16().flat_map(|u| u.to_be_bytes())).collect();
        assert_eq!(decode(&be).unwrap(), s);
        // Windows-1252: Ø = D8, en dash = 96, degree = B0
        assert_eq!(decode(b"PSV-1203 \xD850 \x96 45\xB0").unwrap(), s);
        // UTF-16 with no mark
        let bare: Vec<u8> = "LINE 6\"-P-1001-A1A\r\n".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(decode(&bare).unwrap(), "LINE 6\"-P-1001-A1A\r\n");
        assert!(decode(&[0u8, 1, 0, 0, 7, 0, 0, 0, 9, 200, 0, 0]).is_err(), "binary is refused, not decoded");
    }

    #[test]
    fn separators() {
        assert_eq!(separator("a,b,c\n"), ',');
        assert_eq!(separator("\n\"x;y\";b;c\n"), ';');
        assert_eq!(separator("TAG\tSERVICE\n"), '\t');
        assert_eq!(separator("one column"), ',');
    }
}
