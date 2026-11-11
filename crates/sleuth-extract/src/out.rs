//! Where every reader puts its lines: pages, the per-page facts, the line
//! limit and the deadline, so each format handles them the same way.

use crate::{Limits, MAX_LINE_CHARS};
use anyhow::Result;
use sleuth_core::protocol::{NormBox, PageInfo, Provenance};
use sleuth_core::text::{FileText, Line, PageText, READER_VERSION};
use std::time::Instant;

pub(crate) struct Out {
    pages: Vec<PageText>,
    info: Vec<PageInfo>,
    lines: usize,
    max_lines: usize,
    deadline: Instant,
    ticks: u32,
}

/// One line of clean text: runs of whitespace become one space, control and
/// invisible formatting characters go, and the ends are trimmed.
pub(crate) fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        if c.is_whitespace() {
            gap = true;
        } else if c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}' | '\u{FFFE}' | '\u{FFFF}' | '\u{AD}') {
            // dropped without breaking the word it sits in
        } else {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(c);
        }
    }
    out
}

/// Cut clean text into pieces of at most `MAX_LINE_CHARS` characters: at a
/// cell separator (` | `) where one sits in the last third of the piece,
/// else at a space there, else where the limit falls.
pub(crate) fn pieces(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let Some((cut, _)) = rest.char_indices().nth(MAX_LINE_CHARS) else {
            if !rest.is_empty() {
                out.push(rest);
            }
            return out;
        };
        let window = &rest[..cut];
        let floor = window.char_indices().nth(MAX_LINE_CHARS * 2 / 3).map(|(i, _)| i).unwrap_or(0);
        // (end of this piece, start of the next)
        let (end, next) = match (window.rfind(" | "), window.rfind(' ')) {
            (Some(i), _) if i >= floor => (i, i + 3),
            (_, Some(i)) if i >= floor => (i, i + 1),
            _ => (cut, cut),
        };
        out.push(rest[..end].trim_end());
        rest = rest[next..].trim_start();
    }
}

impl Out {
    pub(crate) fn new(limits: &Limits, deadline: Instant) -> Out {
        Out { pages: Vec::new(), info: Vec::new(), lines: 0, max_lines: limits.max_lines, deadline, ticks: 0 }
    }

    /// Start the next page (sheet, slide, document, model).
    pub(crate) fn page(&mut self, class: &str, note: &str) {
        let page = self.pages.len() as u32 + 1;
        self.pages.push(PageText { page, lines: Vec::new() });
        self.info.push(PageInfo { page, class: class.into(), note: note.into(), ..Default::default() });
    }

    /// Add to the current page's note (what was left out, and why).
    pub(crate) fn note(&mut self, text: &str) {
        if self.info.is_empty() {
            self.page("text", "");
        }
        if let Some(info) = self.info.last_mut() {
            if !info.note.is_empty() {
                info.note.push_str("; ");
            }
            info.note.push_str(text);
        }
    }

    /// The line limit is reached: readers stop adding.
    pub(crate) fn full(&self) -> bool {
        self.lines >= self.max_lines
    }

    /// Lines that still fit.
    pub(crate) fn room(&self) -> usize {
        self.max_lines.saturating_sub(self.lines)
    }

    /// Called from the parsers' loops: an error once the deadline passes.
    pub(crate) fn tick(&mut self) -> Result<()> {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.is_multiple_of(512) && Instant::now() >= self.deadline {
            anyhow::bail!("timed out");
        }
        Ok(())
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Add one line of text with no place on the page. Returns false once
    /// the line limit is reached.
    pub(crate) fn line(&mut self, text: &str) -> bool {
        self.line_at(text, NormBox::default())
    }

    /// Add one line of text at a known place.
    pub(crate) fn line_at(&mut self, text: &str, bbox: NormBox) -> bool {
        let text = clean(text);
        if text.is_empty() {
            return !self.full();
        }
        if self.pages.is_empty() {
            self.page("text", "");
        }
        for piece in pieces(&text) {
            if self.full() {
                return false;
            }
            let (Some(page), Some(info)) = (self.pages.last_mut(), self.info.last_mut()) else { return false };
            info.chars += piece.chars().count() as u32;
            page.lines.push(Line { bbox, ..Line::plain(piece) });
            self.lines += 1;
        }
        !self.full()
    }

    /// One page holding what a Windows text filter returned.
    pub(crate) fn filter_page(&mut self, class: &str, lines: &[String]) {
        self.page(class, "");
        for l in lines {
            if !self.line(l) {
                break;
            }
        }
    }

    pub(crate) fn finish(mut self) -> FileText {
        if self.full() && self.max_lines > 0 {
            let n = self.lines;
            self.note(&format!("stopped after {n} lines"));
        }
        FileText { reader_version: READER_VERSION, model: String::new(), pages: self.pages, page_info: self.info, provenance: Provenance::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning() {
        assert_eq!(clean("  PSV-1203 \t\r\n  SET\u{a0}PRESSURE\u{7}  "), "PSV-1203 SET PRESSURE");
        assert_eq!(clean("\u{feff}ISO\u{200b}-1001"), "ISO-1001");
        assert_eq!(clean(" \u{1} "), "");
    }

    #[test]
    fn long_lines_are_cut_and_continued() {
        let word = "ISO-1001-01 ";
        let long = clean(&word.repeat(300));
        let p = pieces(&long);
        assert!(p.len() >= 4);
        assert!(p.iter().all(|s| s.chars().count() <= MAX_LINE_CHARS && !s.is_empty()));
        assert!(p.iter().all(|s| s.starts_with("ISO-1001-01") && s.ends_with("ISO-1001-01")), "cut at spaces, never inside a tag");
        assert_eq!(p.join(" "), long);
        // no space to cut at: a hard cut, nothing lost
        let solid = "X".repeat(2500);
        let p = pieces(&solid);
        assert_eq!(p.iter().map(|s| s.len()).collect::<Vec<_>>(), vec![1000, 1000, 500]);
        // multi-byte characters are never split
        let wide = "Ø".repeat(1001);
        assert_eq!(pieces(&wide).iter().map(|s| s.chars().count()).collect::<Vec<_>>(), vec![1000, 1]);
    }

    #[test]
    fn line_limit_is_said_in_the_note() {
        let limits = Limits { max_lines: 3, ..Default::default() };
        let mut out = Out::new(&limits, Instant::now() + limits.timeout);
        out.page("sheet", "Lines");
        for i in 0..10 {
            if !out.line(&format!("row {i}")) {
                break;
            }
        }
        let t = out.finish();
        assert_eq!(t.pages[0].lines.len(), 3);
        assert_eq!(t.page_info[0].note, "Lines; stopped after 3 lines");
        assert_eq!(t.page_info[0].chars, 15);
        assert_eq!(t.reader_version, READER_VERSION);
    }
}
