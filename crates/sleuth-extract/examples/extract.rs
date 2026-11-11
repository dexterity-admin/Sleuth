//! Read one file and say what came out.
//!
//! ```text
//! extract <file> [--show] [--timeout <seconds>]
//! extract --list <file-of-paths> [--timeout <seconds>] [--compare-filter] [--dump <folder>]
//! ```
//!
//! Prints the kind, the pages, the line count and the time. The first five
//! lines are printed only with `--show`. With `--list`, every path in the
//! given text file is read and one result row is printed per file, by its
//! number in the list (never its name), followed by totals per kind.
//!
//! `--compare-filter` also reads each file through the Windows text filter
//! registered for its type, where there is one, and counts how many of our
//! lines the filter's text contains: a check of the built-in readers against
//! an independent one.
//!
//! `--dump <folder>` writes every line read to `<n>.ours.txt` there (and the
//! filter's lines to `<n>.filter.txt`), for checking a reading by hand.

use sleuth_core::kinds::FileKind;
use sleuth_core::text::FileText;
use sleuth_extract::{Extracted, Limits, Tools, abilities, extract, ifilter};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

struct Outcome {
    kind: FileKind,
    /// "read", "not here" or "error".
    status: &'static str,
    pages: usize,
    lines: usize,
    chars: u64,
    placed: usize,
    millis: u128,
    detail: String,
    notes: Vec<String>,
    /// The notes without sheet and slide names (list mode shows no names).
    remarks: Vec<String>,
    text: Option<FileText>,
}

fn run(path: &Path, tools: &Tools, limits: &Limits) -> Outcome {
    let kind = FileKind::of(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let t = Instant::now();
    let result = extract(path, kind, tools, limits);
    let millis = t.elapsed().as_millis();
    let mut o = Outcome { kind, status: "error", pages: 0, lines: 0, chars: 0, placed: 0, millis, detail: String::new(), notes: Vec::new(), remarks: Vec::new(), text: None };
    match result {
        Ok(Extracted::Text(text)) => {
            o.status = "read";
            o.pages = text.pages.len();
            o.lines = text.pages.iter().map(|p| p.lines.len()).sum();
            o.chars = text.page_info.iter().map(|p| u64::from(p.chars)).sum();
            o.placed = text.pages.iter().flat_map(|p| &p.lines).filter(|l| l.bbox.x != 0.0 || l.bbox.y != 0.0).count();
            o.notes = text.page_info.iter().filter(|p| !p.note.is_empty()).map(|p| format!("page {} ({}): {}", p.page, p.class, p.note)).collect();
            // A sheet's or slide's note begins with its name; what follows
            // is what the reader had to say about it.
            o.remarks = text
                .page_info
                .iter()
                .filter_map(|p| match p.class.as_str() {
                    "sheet" | "slide" => p.note.split_once("; ").map(|(_, r)| r.to_string()),
                    _ => Some(p.note.clone()),
                })
                .filter(|r| !r.is_empty())
                .collect();
            o.text = Some(text);
        }
        Ok(Extracted::NotHere(why)) => {
            o.status = "not here";
            o.detail = why;
        }
        Err(e) => o.detail = format!("{e:#}"),
    }
    o
}

/// Letters and digits only, so that spacing and line breaks do not matter.
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_uppercase).collect()
}

/// Write lines to `<folder>\<n>.<what>.txt`.
fn dump(folder: Option<&Path>, n: usize, what: &str, lines: &[&str]) {
    if let Some(folder) = folder {
        let _ = std::fs::create_dir_all(folder);
        if let Err(e) = std::fs::write(folder.join(format!("{n}.{what}.txt")), lines.join("\n")) {
            eprintln!("cannot write the dump: {e}");
        }
    }
}

fn dump_ours(folder: Option<&Path>, n: usize, text: &FileText) {
    let lines: Vec<&str> = text.pages.iter().flat_map(|p| &p.lines).map(|l| l.text.as_str()).collect();
    dump(folder, n, "ours", &lines);
}

/// How many of our lines the registered text filter also gives.
fn compare_filter(path: &Path, ours: &FileText, limits: &Limits, folder: Option<&Path>, n: usize) -> String {
    match ifilter::ifilter_text(path, Instant::now() + limits.timeout) {
        Ok(Some(lines)) => {
            dump(folder, n, "filter", &lines.iter().map(String::as_str).collect::<Vec<_>>());
            let theirs = squash(&lines.join(" "));
            let mine: Vec<String> = ours.pages.iter().flat_map(|p| &p.lines).map(|l| squash(&l.text)).filter(|l| !l.is_empty()).collect();
            // Searching a long text for every line is slow; large files are
            // compared from their dumps instead.
            if theirs.len().saturating_mul(mine.len()) > 4_000_000_000 {
                return format!("filter: {} lines; too large to compare here ({} of ours)", lines.len(), mine.len());
            }
            let shared = mine.iter().filter(|l| theirs.contains(l.as_str())).count();
            format!("filter: {} lines; {} of our {} lines are in its text", lines.len(), shared, mine.len())
        }
        Ok(None) => "filter: none registered for this type".into(),
        Err(e) => format!("filter: {e:#}"),
    }
}

fn main() {
    let mut show = false;
    let mut compare = false;
    let mut list: Option<String> = None;
    let mut dump_to: Option<std::path::PathBuf> = None;
    let mut files: Vec<String> = Vec::new();
    let mut limits = Limits::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--show" => show = true,
            "--compare-filter" => compare = true,
            "--list" => list = args.next(),
            "--dump" => dump_to = args.next().map(Into::into),
            "--timeout" => {
                if let Some(s) = args.next().and_then(|s| s.parse().ok()) {
                    limits.timeout = Duration::from_secs(s);
                }
            }
            _ => files.push(a),
        }
    }
    let tools = Tools::find();
    let can = abilities(&tools);
    println!("abilities: office filter {}, dgn filter {}, dwg {}", can.office_filter, can.dgn_filter, can.dwg);

    if let Some(list) = list {
        let Ok(text) = std::fs::read_to_string(&list) else {
            eprintln!("cannot read the list");
            std::process::exit(2);
        };
        // kind → (files, read, not here, errors, lines, millis)
        let mut totals: BTreeMap<&'static str, (u32, u32, u32, u32, usize, u128)> = BTreeMap::new();
        for (i, path) in text.lines().map(str::trim).filter(|l| !l.is_empty()).enumerate() {
            let o = run(Path::new(path), &tools, &limits);
            let notes = if o.remarks.is_empty() { String::new() } else { format!("  [{}]", o.remarks.join("; ")) };
            println!("{:>4} {:<6} {:<8} pages {:<3} lines {:<7} placed {:<7} chars {:<9} {:>6} ms  {}{}", i + 1, o.kind.label(), o.status, o.pages, o.lines, o.placed, o.chars, o.millis, o.detail, notes);
            if let Some(t) = &o.text {
                dump_ours(dump_to.as_deref(), i + 1, t);
                if compare {
                    println!("     {}", compare_filter(Path::new(path), t, &limits, dump_to.as_deref(), i + 1));
                }
            }
            let t = totals.entry(o.kind.label()).or_default();
            t.0 += 1;
            match o.status {
                "read" => t.1 += 1,
                "not here" => t.2 += 1,
                _ => t.3 += 1,
            }
            t.4 += o.lines;
            t.5 += o.millis;
        }
        for (kind, (files, read, not_here, errors, lines, millis)) in totals {
            println!("{kind}: {files} files, {read} read, {not_here} not here, {errors} errors, {lines} lines, {millis} ms");
        }
        return;
    }

    if files.is_empty() {
        eprintln!("usage: extract <file> [--show] [--timeout <seconds>] | extract --list <file-of-paths> [--compare-filter] [--dump <folder>]");
        std::process::exit(2);
    }
    for (i, f) in files.iter().enumerate() {
        let o = run(Path::new(f), &tools, &limits);
        println!("kind: {}", o.kind.label());
        println!("result: {}{}{}", o.status, if o.detail.is_empty() { "" } else { ": " }, o.detail);
        println!("pages: {}", o.pages);
        println!("lines: {} ({} with a position)", o.lines, o.placed);
        println!("characters: {}", o.chars);
        println!("time: {} ms", o.millis);
        for n in &o.notes {
            println!("note: {n}");
        }
        if let Some(t) = &o.text {
            dump_ours(dump_to.as_deref(), i + 1, t);
            if compare {
                println!("{}", compare_filter(Path::new(f), t, &limits, dump_to.as_deref(), i + 1));
            }
        }
        if let (true, Some(t)) = (show, &o.text) {
            for l in t.pages.iter().flat_map(|p| &p.lines).take(5) {
                println!("  {}", l.text);
            }
        }
    }
}
