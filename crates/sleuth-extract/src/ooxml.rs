//! Word and PowerPoint files (.docx .docm .pptx .pptm): a zip of XML parts.
//!
//! Both formats mark text the same way: paragraphs (`p`) of runs with text
//! (`t`), tables as rows (`tr`) of cells (`tc`). A paragraph is one line; a
//! table row is one line with its cells joined by ` | `. Elements are matched
//! by local name, so a file using other namespace prefixes reads the same.
//!
//! Not read: text drawn in charts and SmartArt (they live in other parts),
//! deleted text of tracked changes, field codes.

use crate::Out;
use crate::out::clean;
use anyhow::{Context, Result};
use quick_xml::{Reader, XmlVersion};
use quick_xml::events::{BytesStart, Event};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use zip::ZipArchive;

type Zip = ZipArchive<BufReader<File>>;

/// One XML part may unpack to this much; more is not a document.
const MAX_PART_BYTES: u64 = 2 << 30;
/// A paragraph buffer this long is written out without waiting for its end.
const FLUSH_BYTES: usize = 1 << 20;

fn open_zip(path: &Path) -> Result<Zip> {
    let f = File::open(path).with_context(|| "cannot open the file")?;
    ZipArchive::new(BufReader::new(f)).map_err(|e| anyhow::anyhow!("damaged: not a readable zip ({e})"))
}

fn names(zip: &Zip) -> Vec<String> {
    zip.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect()
}

fn part<'a>(zip: &'a mut Zip, name: &str) -> Result<impl BufRead + 'a> {
    let f = zip.by_name(name).map_err(|e| anyhow::anyhow!("damaged: {name} cannot be read ({e})"))?;
    if f.size() > MAX_PART_BYTES {
        anyhow::bail!("too large: {name} unpacks to {} MB", f.size() >> 20);
    }
    Ok(BufReader::new(f.take(MAX_PART_BYTES)))
}

/// The named attribute's value, matched by local name (`r:id` → `id`).
fn attr(e: &BytesStart, local: &str) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.local_name().as_ref() == local).and_then(|a| a.normalized_value(XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

/// The relationships of a part: (id, type, target).
fn rels(zip: &mut Zip, name: &str) -> Vec<(String, String, String)> {
    let mut found = Vec::new();
    let Ok(r) = part(zip, name) else { return found };
    let mut xml = Reader::from_reader(r);
    let mut buf = Vec::new();
    loop {
        match xml.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "Relationship" => {
                if let (Some(id), Some(ty), Some(target)) = (attr(&e, "Id"), attr(&e, "Type"), attr(&e, "Target")) {
                    found.push((id, ty, target));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    found
}

/// `ppt/slides/slide1.xml` → `ppt/slides/_rels/slide1.xml.rels`.
fn rels_name(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// A relationship target as a zip entry name: relative to the part's folder
/// unless it starts at the root.
fn resolve(part: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut segs: Vec<&str> = part.split('/').collect();
    segs.pop();
    for s in target.split('/') {
        match s {
            ".." => {
                segs.pop();
            }
            "." | "" => {}
            s => segs.push(s),
        }
    }
    segs.join("/")
}

/// The main part named by the package relationships, when it is there.
fn main_part(zip: &mut Zip, all: &[String], fallback: &str) -> Option<String> {
    let named = rels(zip, "_rels/.rels").into_iter().find(|(_, ty, _)| ty.ends_with("/officeDocument")).map(|(_, _, t)| resolve("", &t));
    named.into_iter().chain([fallback.to_string()]).find(|n| all.iter().any(|a| a == n))
}

struct Table {
    cells: Vec<String>,
    cell: Option<String>,
}

/// Collects the lines of one XML part.
struct Walker<'a> {
    out: &'a mut Out,
    paras: Vec<String>,
    tables: Vec<Table>,
    /// Text outside any paragraph.
    loose: String,
    in_text: bool,
    /// Depth inside an element whose content is not text of the document.
    skip: usize,
    /// Lines already given (headers repeat on every section).
    seen: Option<HashSet<String>>,
}

impl Walker<'_> {
    fn emit(&mut self, text: &str) {
        for l in text.split('\n') {
            let l = clean(l);
            if l.is_empty() {
                continue;
            }
            if let Some(seen) = self.seen.as_mut()
                && !seen.insert(l.clone())
            {
                continue;
            }
            self.out.line(&l);
        }
    }

    fn push(&mut self, s: &str) {
        let alone = self.tables.is_empty() && self.paras.len() <= 1;
        let top = self.paras.last_mut().unwrap_or(&mut self.loose);
        top.push_str(s);
        if alone && top.len() > FLUSH_BYTES {
            let text = std::mem::take(top);
            self.emit(&text);
        }
    }

    /// A finished paragraph: into the table cell it sits in, or a line.
    fn end_para(&mut self) {
        let Some(text) = self.paras.pop() else { return };
        if let Some(cell) = self.tables.last_mut().and_then(|t| t.cell.as_mut()) {
            cell.push_str(&text);
            cell.push('\n');
        } else {
            self.emit(&text);
        }
    }

    fn start(&mut self, e: &BytesStart, empty: bool) {
        if self.skip > 0 {
            if !empty {
                self.skip += 1;
            }
            return;
        }
        match e.local_name().as_ref() {
            "p" if !empty => self.paras.push(String::new()),
            "t" if !empty => self.in_text = true,
            "br" | "cr" => self.push("\n"),
            "tab" | "ptab" => self.push("\t"),
            "noBreakHyphen" => self.push("-"),
            "tbl" if !empty => self.tables.push(Table { cells: Vec::new(), cell: None }),
            "tr" => {
                if let Some(t) = self.tables.last_mut() {
                    t.cells.clear();
                }
            }
            "tc" if !empty => {
                if let Some(t) = self.tables.last_mut() {
                    t.cell = Some(String::new());
                }
            }
            // The older rendering of the same shape, deleted text, field
            // codes, and the slide-number field of a slide.
            "Fallback" | "delText" | "instrText" | "delInstrText" if !empty => self.skip = 1,
            "fld" if !empty && attr(e, "type").is_some_and(|t| t == "slidenum") => self.skip = 1,
            _ => {}
        }
    }

    fn end(&mut self, local: &str) {
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        match local {
            "p" => self.end_para(),
            "t" => self.in_text = false,
            "tc" => {
                if let Some(t) = self.tables.last_mut()
                    && let Some(c) = t.cell.take()
                {
                    t.cells.push(c);
                }
            }
            "tr" => {
                if let Some(t) = self.tables.last_mut() {
                    let row = t.cells.iter().map(|c| clean(c)).filter(|c| !c.is_empty()).collect::<Vec<_>>().join(" | ");
                    t.cells.clear();
                    self.emit(&row);
                }
            }
            "tbl" => {
                self.tables.pop();
            }
            _ => {}
        }
    }

    fn finish(&mut self) {
        while !self.paras.is_empty() {
            self.end_para();
        }
        let loose = std::mem::take(&mut self.loose);
        self.emit(&loose);
    }
}

/// The predefined entities; anything else named is not text we know.
fn entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => return None,
    })
}

/// Walk one part's XML, adding its lines to the current page.
fn walk(reader: impl BufRead, out: &mut Out, seen: Option<HashSet<String>>) -> Result<Option<HashSet<String>>> {
    let mut xml = Reader::from_reader(reader);
    let mut w = Walker { out, paras: Vec::new(), tables: Vec::new(), loose: String::new(), in_text: false, skip: 0, seen };
    let mut buf = Vec::new();
    loop {
        w.out.tick()?;
        if w.out.full() {
            break;
        }
        match xml.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => w.start(&e, false),
            Ok(Event::Empty(e)) => w.start(&e, true),
            Ok(Event::End(e)) => w.end(e.local_name().as_ref()),
            Ok(Event::Text(t)) if w.in_text && w.skip == 0 => w.push(t.as_ref()),
            Ok(Event::CData(t)) if w.in_text && w.skip == 0 => w.push(t.as_ref()),
            Ok(Event::GeneralRef(r)) if w.in_text && w.skip == 0 => match r.resolve_char_ref() {
                Ok(Some(c)) => w.push(c.encode_utf8(&mut [0u8; 4])),
                _ => {
                    if let Some(s) = entity(r.as_ref()) {
                        w.push(s);
                    }
                }
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                w.finish();
                w.out.note(&format!("the XML is damaged; text after that point is missing ({e})"));
                return Ok(w.seen.take());
            }
        }
        buf.clear();
    }
    w.finish();
    Ok(w.seen.take())
}

/// `header12.xml` sorts after `header2.xml`.
fn numbered(names: &[String], dir: &str, stem: &str) -> Vec<String> {
    let mut found: Vec<(u32, String)> = names
        .iter()
        .filter_map(|n| {
            let rest = n.strip_prefix(dir)?.strip_prefix(stem)?.strip_suffix(".xml")?;
            if rest.is_empty() { Some((0, n.clone())) } else { rest.parse().ok().map(|i| (i, n.clone())) }
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, n)| n).collect()
}

pub fn read_docx(path: &Path, out: &mut Out) -> Result<()> {
    let mut zip = open_zip(path)?;
    let all = names(&zip);
    let Some(main) = main_part(&mut zip, &all, "word/document.xml") else {
        anyhow::bail!("damaged or not a Word document: it has no document part");
    };
    out.page("document", "");
    walk(part(&mut zip, &main)?, out, None)?;
    let dir = main.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
    // Headers and footers repeat per section; each distinct line once.
    let mut seen = Some(HashSet::new());
    for stem in ["header", "footer"] {
        for name in numbered(&all, &dir, stem) {
            seen = walk(part(&mut zip, &name)?, out, seen)?;
        }
    }
    for name in ["footnotes.xml", "endnotes.xml", "comments.xml"] {
        let name = format!("{dir}{name}");
        if all.contains(&name) {
            walk(part(&mut zip, &name)?, out, None)?;
        }
    }
    Ok(())
}

/// The slides in presentation order: (part name).
fn slide_order(zip: &mut Zip, all: &[String], main: &str) -> Vec<String> {
    let map = rels(zip, &rels_name(main));
    let mut order = Vec::new();
    if let Ok(r) = part(zip, main) {
        let mut xml = Reader::from_reader(r);
        let mut buf = Vec::new();
        loop {
            match xml.read_event_into(&mut buf) {
                Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "sldId" => {
                    // `id` is the slide's number; the relationship is `r:id`.
                    let rid = e.attributes().flatten().find(|a| a.key.as_ref().ends_with(":id")).and_then(|a| a.normalized_value(XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()));
                    if let Some(target) = rid.and_then(|rid| map.iter().find(|(id, _, _)| *id == rid)).map(|(_, _, t)| resolve(main, t))
                        && all.contains(&target)
                    {
                        order.push(target);
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
            buf.clear();
        }
    }
    if order.is_empty() {
        let dir = main.rsplit_once('/').map(|(d, _)| format!("{d}/slides/")).unwrap_or_else(|| "slides/".into());
        order = numbered(all, &dir, "slide");
    }
    order
}

pub fn read_pptx(path: &Path, out: &mut Out) -> Result<()> {
    let mut zip = open_zip(path)?;
    let all = names(&zip);
    let Some(main) = main_part(&mut zip, &all, "ppt/presentation.xml") else {
        anyhow::bail!("damaged or not a presentation: it has no presentation part");
    };
    let slides = slide_order(&mut zip, &all, &main);
    for (i, slide) in slides.iter().enumerate() {
        out.tick()?;
        out.page("slide", &format!("Slide {}", i + 1));
        if out.full() {
            continue;
        }
        walk(part(&mut zip, slide)?, out, None)?;
        let notes = rels(&mut zip, &rels_name(slide)).into_iter().find(|(_, ty, _)| ty.ends_with("/notesSlide")).map(|(_, _, t)| resolve(slide, &t));
        if let Some(notes) = notes.filter(|n| all.contains(n)) {
            walk(part(&mut zip, &notes)?, out, None)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_resolve() {
        assert_eq!(resolve("ppt/presentation.xml", "slides/slide1.xml"), "ppt/slides/slide1.xml");
        assert_eq!(resolve("ppt/slides/slide1.xml", "../notesSlides/notesSlide1.xml"), "ppt/notesSlides/notesSlide1.xml");
        assert_eq!(resolve("ppt/presentation.xml", "/ppt/slides/slide2.xml"), "ppt/slides/slide2.xml");
        assert_eq!(resolve("", "word/document.xml"), "word/document.xml");
        assert_eq!(rels_name("ppt/slides/slide1.xml"), "ppt/slides/_rels/slide1.xml.rels");
    }

    #[test]
    fn parts_sort_by_number() {
        let all: Vec<String> = ["word/header10.xml", "word/header2.xml", "word/header.xml", "word/headers.xml", "word/footer1.xml"].iter().map(|s| s.to_string()).collect();
        assert_eq!(numbered(&all, "word/", "header"), ["word/header.xml", "word/header2.xml", "word/header10.xml"]);
    }
}
