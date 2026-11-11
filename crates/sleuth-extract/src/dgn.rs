//! MicroStation V7 design files (`.dgn`, and the PDS isometric natives
//! `.i01` … `.i99`): the Intergraph Standard File Format element stream.
//!
//! A V7 file is a run of elements. Each starts with two 16-bit words: the
//! level and type (with a "complex" bit and a "deleted" bit), and the number
//! of words that follow. Displayable elements then carry their range, six
//! 32-bit integers in design units (low x, y, z, high x, y, z). The file
//! begins with the design-file header (type 9), sometimes after a type 8
//! element, and ends with the word `FFFF`.
//!
//! Taken as lines: text elements (type 17, including the lines of text
//! nodes), string tag values (type 37) and each distinct cell name (type 2).
//! Every line's box is its element's range against the union of the ranges
//! of all displayable elements, y down. In a 3D file that is the top view.
//!
//! Not read: dimension text (type 33), text inside shared-cell definitions
//! keeps no position, numeric tag values, and multi-byte text whose code
//! page the file does not name.

use crate::Out;
use crate::cad::Extents;
use anyhow::{Context, Result};
use encoding_rs::WINDOWS_1252;
use std::collections::HashSet;
use std::path::Path;

const T_CELL: u8 = 2;
const T_DIGITIZER: u8 = 8;
const T_TCB: u8 = 9;
const T_TEXT: u8 = 17;
const T_SHARED_CELL_DEF: u8 = 34;
const T_TAG: u8 = 37;

/// A 32-bit integer as V7 stores it: the high word first, each word
/// little-endian.
fn int32(b: &[u8], at: usize) -> Option<u32> {
    let w = b.get(at..at + 4)?;
    Some(u32::from(w[2]) | u32::from(w[3]) << 8 | u32::from(w[0]) << 16 | u32::from(w[1]) << 24)
}

/// A range coordinate: stored with the sign bit flipped so that ranges
/// compare as unsigned numbers.
fn range_coord(b: &[u8], at: usize) -> Option<f64> {
    int32(b, at).map(|v| f64::from((v ^ 0x8000_0000) as i32))
}

/// Element types that are drawn and so carry a meaningful range.
fn displayable(ty: u8) -> bool {
    matches!(ty, 2..=4 | 6 | 7 | 11..=19 | 21..=28 | 33 | 35 | 37 | 87 | 88)
}

/// Whether the bytes are a V7 design file: they begin with the design-file
/// header (type 9, on level 8), or with a type 8 element and the header
/// right after it.
pub fn is_v7(b: &[u8]) -> bool {
    let header_at = |at: usize| b.get(at..at.saturating_add(4)).is_some_and(|h| h[1] & 0x7F == T_TCB && h[0] & 0x3F == 8 && u16::from_le_bytes([h[2], h[3]]) >= 0x0100);
    match b.get(..4) {
        Some(_) if header_at(0) => true,
        Some(h) if h[1] & 0x7F == T_DIGITIZER => header_at((usize::from(u16::from_le_bytes([h[2], h[3]])) + 2) * 2),
        _ => false,
    }
}

/// The Radix-50 alphabet as Intergraph's own design-file editor (EDG) has
/// it: code 29 is an underscore here, not DEC's `%`.
const RAD50: &[u8; 40] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ$._0123456789";

/// Three characters of a cell name packed in Radix-50. `None` when the word
/// is not valid Radix-50.
fn rad50(word: u16) -> Option<[char; 3]> {
    if word >= 40 * 40 * 40 {
        return None;
    }
    let one = |v: u16| RAD50[usize::from(v % 40)] as char;
    Some([one(word / 1600), one(word / 40), one(word)])
}

/// The characters of a text element. Two-byte text (it starts `FF FD`) is
/// kept only where a character is in the single-byte range.
fn text_of(bytes: &[u8]) -> String {
    if let Some(wide) = bytes.strip_prefix(&[0xFF, 0xFD]) {
        return wide
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| match u16::from_le_bytes(*c) {
                0 => ' ',
                w @ 1..=0xFF => WINDOWS_1252.decode_without_bom_handling(&[w as u8]).0.chars().next().unwrap_or('\u{fffd}'),
                _ => '\u{fffd}',
            })
            .collect();
    }
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    WINDOWS_1252.decode_without_bom_handling(&bytes[..end]).0.into_owned()
}

struct Found {
    text: String,
    /// The element's range (low, high) in design units; `None` inside a
    /// shared-cell definition, whose coordinates are the cell's own.
    range: Option<((f64, f64), (f64, f64))>,
}

/// Everything read from the element stream before positions are worked out.
#[derive(Default)]
struct Scan {
    found: Vec<Found>,
    elements: usize,
    deleted: usize,
    /// Why the walk stopped before the end marker, if it did.
    stopped: Option<String>,
    two_byte: bool,
}

fn scan(b: &[u8], out: &mut Out, limit: usize) -> Result<(Scan, Extents)> {
    let mut s = Scan::default();
    let mut extents = Extents::empty();
    let mut three_d = false;
    let mut seen_tcb = false;
    let mut in_shared_def = false;
    let mut cells: HashSet<String> = HashSet::new();
    let mut off = 0usize;
    while off + 4 <= b.len() {
        out.tick()?;
        if b[off] == 0xFF && b[off + 1] == 0xFF {
            break; // end of design
        }
        let ty = b[off + 1] & 0x7F;
        let deleted = b[off + 1] & 0x80 != 0;
        let complex = b[off] & 0x80 != 0;
        let size = (usize::from(u16::from_le_bytes([b[off + 2], b[off + 3]])) + 2) * 2;
        let Some(e) = b.get(off..off + size) else {
            s.stopped = Some(format!("the file ends inside element {}", s.elements + 1));
            break;
        };
        off += size;
        s.elements += 1;
        if ty == T_TCB {
            if !seen_tcb {
                // The header says whether coordinates have a z.
                three_d = e.get(1214).is_some_and(|f| f & 0x40 != 0);
                seen_tcb = true;
            }
            continue;
        }
        if deleted {
            s.deleted += 1;
            continue;
        }
        if ty == T_SHARED_CELL_DEF {
            in_shared_def = true;
        } else if !complex {
            in_shared_def = false;
        }
        if !displayable(ty) && ty != T_SHARED_CELL_DEF {
            continue;
        }
        let range = (|| {
            let lo = (range_coord(e, 4)?, range_coord(e, 8)?);
            let hi = (range_coord(e, 16)?, range_coord(e, 20)?);
            (lo.0 <= hi.0 && lo.1 <= hi.1).then_some((lo, hi))
        })();
        let placed = range.filter(|_| !in_shared_def && displayable(ty));
        if let Some((lo, hi)) = placed {
            extents.add(lo);
            extents.add(hi);
        }
        let text = match ty {
            T_TEXT => {
                // font, justification, two multipliers, then rotation and
                // origin (2D) or quaternion and origin (3D), then the count.
                let count_at = if three_d { 74 } else { 58 };
                let Some(&n) = e.get(count_at) else { continue };
                let start = count_at + 2;
                let Some(chars) = e.get(start..start + usize::from(n)) else { continue };
                s.two_byte |= chars.starts_with(&[0xFF, 0xFD]);
                text_of(chars)
            }
            T_TAG => {
                // Value type at 74 (1 = string), the value from 154.
                if e.get(74..76) != Some(&[1u8, 0][..]) {
                    continue;
                }
                let Some(value) = e.get(154..) else { continue };
                text_of(value)
            }
            T_CELL => {
                let name = (|| {
                    let a = rad50(u16::from_le_bytes([*e.get(38)?, *e.get(39)?]))?;
                    let c = rad50(u16::from_le_bytes([*e.get(40)?, *e.get(41)?]))?;
                    Some(a.iter().chain(c.iter()).collect::<String>().trim().to_string())
                })();
                match name {
                    Some(n) if !n.is_empty() && cells.insert(n.clone()) => n,
                    _ => continue,
                }
            }
            _ => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        if s.found.len() >= limit {
            s.stopped = Some(String::new());
            break;
        }
        s.found.push(Found { text, range: placed });
    }
    if !seen_tcb {
        anyhow::bail!("damaged or not a MicroStation V7 file: it has no design-file header");
    }
    Ok((s, extents))
}

/// Read a V7 design file already in memory.
pub(crate) fn read_bytes(b: &[u8], out: &mut Out, limit: usize) -> Result<()> {
    if !is_v7(b) {
        anyhow::bail!("not a MicroStation design file: it does not begin with a design-file header");
    }
    let (s, extents) = scan(b, out, limit)?;
    out.page("model", "");
    for f in &s.found {
        let bbox = f.range.and_then(|(lo, hi)| extents.norm(lo, hi)).unwrap_or_default();
        if !out.line_at(&f.text, bbox) {
            break;
        }
    }
    if let Some(why) = s.stopped.filter(|w| !w.is_empty()) {
        out.note(&format!("{why}; text after that point is missing"));
    }
    if s.two_byte {
        out.note("two-byte text is present; characters outside the single-byte range are shown as \u{fffd}");
    }
    Ok(())
}

pub fn read(path: &Path, out: &mut Out) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| "cannot read the file")?;
    // Collect a little past the limit so the collector reports the cut.
    let limit = out.room().saturating_add(1);
    read_bytes(&bytes, out, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_and_names() {
        // 0x12345678 is stored as 34 12 78 56
        assert_eq!(int32(&[0x34, 0x12, 0x78, 0x56], 0), Some(0x1234_5678));
        // design coordinate 0 is stored as 0x8000_0000
        assert_eq!(range_coord(&[0x00, 0x80, 0x00, 0x00], 0), Some(0.0));
        assert_eq!(range_coord(&[0xFF, 0x7F, 0xFF, 0xFF], 0), Some(-1.0));
        // "GAT" = 7*1600 + 1*40 + 20
        assert_eq!(rad50(7 * 1600 + 40 + 20), Some(['G', 'A', 'T']));
        assert_eq!(rad50(2 * 1600 + 29 * 40 + 31), Some(['B', '_', '1']));
        assert_eq!(rad50(64_000), None);
    }

    #[test]
    fn detection() {
        assert!(is_v7(&[0x08, 0x09, 0xFE, 0x02]), "2D header");
        assert!(is_v7(&[0xC8, 0x09, 0xFE, 0x02]), "3D header");
        // a type 8 element of two words, then the header
        assert!(is_v7(&[0x08, 0x08, 0x02, 0x00, 0, 0, 0, 0, 0x08, 0x09, 0xFE, 0x02]));
        assert!(!is_v7(&[0x08, 0x08, 0x02, 0x00, 0, 0, 0, 0, b'w', b'x', b'y', b'z']), "type 8 alone is not enough");
        assert!(!is_v7(&[0xD0, 0xCF, 0x11, 0xE0]), "V8 is a compound file");
        assert!(!is_v7(b"PK\x03\x04"));
        assert!(!is_v7(b"AC1032"));
    }

    #[test]
    fn two_byte_text_is_not_guessed() {
        assert_eq!(text_of(&[0xFF, 0xFD, b'A', 0, 0x34, 0x12, b'1', 0]), "A\u{fffd}1");
        assert_eq!(text_of(b"P-101A\0"), "P-101A");
        assert_eq!(text_of(&[0xD8, b'5', b'0']), "\u{d8}50");
    }
}
