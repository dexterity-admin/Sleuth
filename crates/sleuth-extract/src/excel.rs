//! Spreadsheets: one page per sheet, one line per row with the cells joined
//! by ` | `, so a tag and its description stay together.
//!
//! The values are the ones stored in the file (a formula gives its last
//! calculated result); number formats are not applied, except that dates
//! come out as dates.

use crate::{OLE_MAGIC, Out, ZIP_MAGIC, encrypted_package, head};
use anyhow::{Context, Result};
use calamine::{Data, DataRef, ExcelDateTime, Ods, Reader, SheetType, Xls, Xlsb, Xlsx};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

type Rs = BufReader<File>;

/// A number as a person would type it: no trailing `.0`, and without the
/// binary noise past the 15 digits a spreadsheet shows.
pub(crate) fn number(f: f64) -> String {
    if !f.is_finite() {
        return String::new();
    }
    if f == f.trunc() && f.abs() < 1e15 {
        return format!("{}", f as i64);
    }
    let rounded: f64 = format!("{f:.14e}").parse().unwrap_or(f);
    if (1e-7..1e15).contains(&rounded.abs()) { format!("{rounded}") } else { format!("{rounded:e}") }
}

/// A date cell as ISO text: a date, a date and time, or a time alone.
fn date(d: &ExcelDateTime) -> String {
    if d.is_duration() {
        let secs = (d.as_f64() * 86_400.0).round() as i64;
        let (sign, secs) = if secs < 0 { ("-", -secs) } else { ("", secs) };
        return format!("{sign}{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    }
    let (y, mo, da, h, mi, s, _) = d.to_ymd_hms_milli();
    let serial = d.as_f64();
    if (0.0..1.0).contains(&serial) {
        format!("{h:02}:{mi:02}:{s:02}")
    } else if h == 0 && mi == 0 && s == 0 {
        format!("{y:04}-{mo:02}-{da:02}")
    } else {
        format!("{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{s:02}")
    }
}

fn cell(d: &Data) -> String {
    match d {
        Data::Int(i) => i.to_string(),
        Data::Float(f) => number(*f),
        Data::String(s) => s.clone(),
        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Data::DateTime(d) => date(d),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        // An error value (#N/A, #REF!) is not text of the document.
        Data::Error(_) | Data::Empty => String::new(),
    }
}

fn cell_ref(d: &DataRef) -> String {
    match d {
        DataRef::Int(i) => i.to_string(),
        DataRef::Float(f) => number(*f),
        DataRef::String(s) => s.clone(),
        DataRef::SharedString(s) => (*s).to_string(),
        DataRef::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        DataRef::DateTime(d) => date(d),
        DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => s.clone(),
        DataRef::Error(_) | DataRef::Empty => String::new(),
    }
}

/// One row's cells as a line. Returns false once the line limit is reached.
fn row(cells: &mut Vec<String>, out: &mut Out) -> bool {
    let line = cells.iter().map(|c| crate::out::clean(c)).filter(|c| !c.is_empty()).collect::<Vec<_>>().join(" | ");
    cells.clear();
    out.line(&line)
}

fn plain_error(e: impl std::fmt::Display) -> anyhow::Error {
    let s = e.to_string();
    if s.to_lowercase().contains("password") { anyhow::anyhow!("password protected") } else { anyhow::anyhow!("damaged or not a workbook: {s}") }
}

/// What a sheet that is not a grid of cells is called in its note.
fn kind_note(t: SheetType) -> Option<&'static str> {
    match t {
        SheetType::WorkSheet => None,
        SheetType::ChartSheet => Some("a chart sheet, no cells"),
        SheetType::DialogSheet => Some("a dialog sheet, no cells"),
        SheetType::MacroSheet => Some("a macro sheet, not read"),
        SheetType::Vba => Some("a VBA module, not read"),
    }
}

/// The formats calamine loads a sheet at a time.
fn ranges<R: Reader<Rs>>(mut wb: R, out: &mut Out) -> Result<()>
where
    R::Error: std::fmt::Display,
{
    let sheets = wb.sheets_metadata().to_vec();
    let mut cells = Vec::new();
    for s in &sheets {
        out.tick()?;
        out.page("sheet", &s.name);
        if let Some(why) = kind_note(s.typ) {
            out.note(why);
            continue;
        }
        if out.full() {
            continue;
        }
        let range = match wb.worksheet_range(&s.name) {
            Ok(r) => r,
            Err(e) => {
                out.note(&format!("this sheet could not be read: {e}"));
                continue;
            }
        };
        for r in range.rows() {
            out.tick()?;
            cells.extend(r.iter().map(cell));
            if !row(&mut cells, out) {
                break;
            }
        }
    }
    Ok(())
}

/// .xlsx is read cell by cell as it streams, so a large sheet never sits in
/// memory whole.
fn xlsx(mut wb: Xlsx<Rs>, out: &mut Out) -> Result<()> {
    let sheets = wb.sheets_metadata().to_vec();
    let mut cells = Vec::new();
    for s in &sheets {
        out.tick()?;
        out.page("sheet", &s.name);
        if let Some(why) = kind_note(s.typ) {
            out.note(why);
            continue;
        }
        if out.full() {
            continue;
        }
        let mut reader = match wb.worksheet_cells_reader(&s.name) {
            Ok(r) => r,
            Err(e) => {
                out.note(&format!("this sheet could not be read: {e}"));
                continue;
            }
        };
        let mut current = 0u32;
        loop {
            out.tick()?;
            match reader.next_cell() {
                Ok(Some(c)) => {
                    let (r, _) = c.get_position();
                    if r != current {
                        if !row(&mut cells, out) {
                            break;
                        }
                        current = r;
                    }
                    cells.push(cell_ref(c.get_value()));
                }
                Ok(None) => {
                    row(&mut cells, out);
                    break;
                }
                Err(e) => {
                    row(&mut cells, out);
                    out.note(&format!("the sheet is damaged after this point: {e}"));
                    break;
                }
            }
        }
        cells.clear();
    }
    Ok(())
}

pub fn read(path: &Path, ext: &str, out: &mut Out) -> Result<()> {
    let magic = head(path, 8)?;
    let open = || -> Result<Rs> { Ok(BufReader::new(File::open(path).with_context(|| "cannot open the file")?)) };
    if magic.starts_with(&OLE_MAGIC) {
        if encrypted_package(path) {
            anyhow::bail!("password protected");
        }
        return ranges(Xls::new(open()?).map_err(plain_error)?, out);
    }
    if magic.starts_with(&ZIP_MAGIC) {
        return match ext {
            "xlsb" => ranges(Xlsb::new(open()?).map_err(plain_error)?, out),
            "ods" => ranges(Ods::new(open()?).map_err(plain_error)?, out),
            "xlsx" | "xlsm" => xlsx(Xlsx::new(open()?).map_err(plain_error)?, out),
            // A zip under another name (.xls is common): whichever it is.
            _ => match Xlsx::new(open()?) {
                Ok(wb) => xlsx(wb, out),
                Err(first) => match Xlsb::new(open()?) {
                    Ok(wb) => ranges(wb, out),
                    Err(_) => match Ods::new(open()?) {
                        Ok(wb) => ranges(wb, out),
                        Err(_) => Err(plain_error(first)),
                    },
                },
            },
        };
    }
    // Reports exported as a web page or as delimited text and named .xls.
    let bytes = std::fs::read(path).with_context(|| "cannot read the file")?;
    let text = crate::plain::decode(&bytes).map_err(|_| anyhow::anyhow!("damaged or not a workbook"))?;
    let start = text.trim_start();
    if start.starts_with('<') {
        let n = start.char_indices().nth(4096).map(|(i, _)| i).unwrap_or(start.len());
        let lower = start[..n].to_ascii_lowercase();
        if lower.contains("<html") || lower.contains("<table") {
            out.page("sheet", "");
            out.note("a web page saved as a workbook");
            for l in crate::email::html_lines(&text) {
                out.tick()?;
                if !out.line(&l) {
                    break;
                }
            }
            return Ok(());
        }
        anyhow::bail!("damaged or not a workbook");
    }
    out.page("sheet", "");
    out.note("delimited text saved as a workbook");
    crate::plain::delimited(&text, '\t', out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_as_typed() {
        assert_eq!(number(150.0), "150");
        assert_eq!(number(-3.0), "-3");
        assert_eq!(number(0.1 + 0.2), "0.3");
        assert_eq!(number(12.5), "12.5");
        assert_eq!(number(1234567.891), "1234567.891");
        assert_eq!(number(1e21), "1e21");
        assert_eq!(number(f64::NAN), "");
    }
}
