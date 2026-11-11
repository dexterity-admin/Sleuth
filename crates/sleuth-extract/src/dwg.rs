//! AutoCAD `.dwg`, read by LibreDWG's `dwgread` run as a separate process.
//!
//! `dwgread -v0 -O minJSON <file>` writes the drawing to its standard output
//! as one JSON object: `HEADER` (variables, among them the extents) and
//! `OBJECTS`, an array with one object per entity. (`minJSON` is `JSON`
//! without the indentation: the same members in two thirds of the bytes.)
//! That array can run to hundreds of megabytes, so it is read as a stream,
//! an entity at a time, and only text-bearing entities are kept. LibreDWG's
//! JSON is not always strict (bare `nan`, raw bytes in strings); the reader
//! here accepts that.
//!
//! Taken as lines: TEXT, MTEXT (formatting codes removed), ATTRIB values,
//! ATTDEF defaults (the tag where there is none), dimension text overrides,
//! and any other entity's text fields (multileaders, tables). Feature control
//! frames (TOLERANCE) are skipped: they hold symbol-font codes, not words.
//! Text in model space is placed against the drawing extents; text in paper
//! space and in block definitions has no place.
//!
//! The process is killed at the deadline. LibreDWG is GPL software; it is
//! run as shipped, never linked.

use crate::Out;
use crate::cad::{Extents, mtext_lines, plain_text};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// ------------------------------------------------------------ JSON stream

/// A JSON value, as much of it as text extraction needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Val {
    /// null, true, false, nan and any other bare word.
    Other,
    Num(f64),
    Str(String),
    Arr(Vec<Val>),
    Obj(Vec<(String, Val)>),
}

impl Val {
    fn get(&self, key: &str) -> Option<&Val> {
        match self {
            Val::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Val::Str(s)) => Some(s),
            _ => None,
        }
    }

    fn num(&self, key: &str) -> Option<f64> {
        match self.get(key) {
            Some(Val::Num(n)) => Some(*n),
            _ => None,
        }
    }

    /// `[x, y, …]` as a point.
    fn point(&self, key: &str) -> Option<(f64, f64)> {
        match self.get(key) {
            Some(Val::Arr(a)) => match (a.first(), a.get(1)) {
                (Some(Val::Num(x)), Some(Val::Num(y))) if x.is_finite() && y.is_finite() => Some((*x, *y)),
                _ => None,
            },
            _ => None,
        }
    }

    /// The last number of a handle array: the handle itself, or for a
    /// reference the absolute handle it points to.
    fn handle(&self, key: &str) -> Option<u64> {
        match self.get(key) {
            Some(Val::Arr(a)) => match a.last() {
                Some(Val::Num(n)) if *n >= 0.0 => Some(*n as u64),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Elements kept from one array, and characters from one string. Entities
/// with long vertex lists or embedded binary data are not held in full.
const KEEP_ITEMS: usize = 512;
const KEEP_STRING: usize = 4 << 20;
const MAX_DEPTH: usize = 96;

/// A forgiving JSON reader over a byte stream.
pub(crate) struct Json<R: Read> {
    src: R,
    buf: Vec<u8>,
    at: usize,
    len: usize,
}

impl<R: Read> Json<R> {
    pub(crate) fn new(src: R) -> Json<R> {
        Json { src, buf: vec![0; 1 << 16], at: 0, len: 0 }
    }

    fn peek(&mut self) -> Option<u8> {
        if self.at == self.len {
            loop {
                match self.src.read(&mut self.buf) {
                    Ok(0) => return None,
                    Ok(n) => {
                        self.at = 0;
                        self.len = n;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return None,
                }
            }
        }
        Some(self.buf[self.at])
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.at += 1;
        Some(c)
    }

    /// The next byte that is not white space, left unread.
    fn ws(&mut self) -> Option<u8> {
        loop {
            match self.peek()? {
                b' ' | b'\t' | b'\r' | b'\n' => self.at += 1,
                c => return Some(c),
            }
        }
    }

    fn fail<T>(&mut self, what: &str) -> Result<T> {
        if self.peek().is_none() {
            anyhow::bail!("the output ends early");
        }
        anyhow::bail!("unexpected output: {what}")
    }

    /// A string whose opening quote is already read. At most `keep` bytes
    /// are kept; the rest is read and dropped.
    fn string(&mut self, keep: usize) -> Result<String> {
        let mut out: Vec<u8> = Vec::new();
        let push = |out: &mut Vec<u8>, bytes: &[u8]| {
            if out.len() < keep {
                out.extend_from_slice(bytes);
            }
        };
        loop {
            let Some(c) = self.bump() else { return self.fail("string") };
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(e) = self.bump() else { return self.fail("string") };
                    match e {
                        b'n' => push(&mut out, b"\n"),
                        b't' => push(&mut out, b"\t"),
                        b'r' => push(&mut out, b"\r"),
                        b'b' | b'f' => push(&mut out, b" "),
                        b'u' => {
                            let first = self.hex4();
                            let ch = match first {
                                // A surrogate pair is one character.
                                Some(hi @ 0xD800..=0xDBFF) => {
                                    let mut ch = None;
                                    if self.peek() == Some(b'\\') {
                                        self.at += 1;
                                        if self.peek() == Some(b'u') {
                                            self.at += 1;
                                            if let Some(lo @ 0xDC00..=0xDFFF) = self.hex4() {
                                                ch = char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
                                            }
                                        }
                                    }
                                    ch
                                }
                                Some(c) => char::from_u32(c),
                                None => None,
                            };
                            let ch = ch.unwrap_or('\u{fffd}');
                            push(&mut out, ch.encode_utf8(&mut [0u8; 4]).as_bytes());
                        }
                        // `\"`, `\\`, `\/`, and anything else escaped as itself.
                        other => push(&mut out, &[other]),
                    }
                }
                c => push(&mut out, &[c]),
            }
        }
        Ok(match String::from_utf8(out) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        })
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = (self.peek()? as char).to_digit(16)?;
            self.at += 1;
            v = v << 4 | d;
        }
        Some(v)
    }

    /// A number or a bare word (`true`, `null`, `nan`, `-inf`).
    fn scalar(&mut self) -> Val {
        let mut word = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.' | b'_') {
                if word.len() < 64 {
                    word.push(c as char);
                }
                self.at += 1;
            } else {
                break;
            }
        }
        match word.parse::<f64>() {
            Ok(n) if n.is_finite() && word.bytes().any(|b| b.is_ascii_digit()) => Val::Num(n),
            _ => Val::Other,
        }
    }

    /// Read past one value without keeping it.
    pub(crate) fn skip(&mut self) -> Result<()> {
        let mut depth = 0usize;
        loop {
            let Some(c) = self.ws() else { return self.fail("value") };
            match c {
                b'{' | b'[' => {
                    self.at += 1;
                    depth += 1;
                }
                b'}' | b']' => {
                    if depth == 0 {
                        return self.fail("closing bracket");
                    }
                    self.at += 1;
                    depth -= 1;
                }
                b'"' => {
                    self.at += 1;
                    self.string(0)?;
                }
                b',' | b':' => {
                    if depth == 0 {
                        return self.fail("separator");
                    }
                    self.at += 1;
                }
                _ => {
                    let before = self.at;
                    self.scalar();
                    if self.at == before {
                        self.at += 1; // a byte that belongs to nothing
                    }
                }
            }
            if depth == 0 {
                return Ok(());
            }
        }
    }

    pub(crate) fn value(&mut self, depth: usize) -> Result<Val> {
        if depth > MAX_DEPTH {
            anyhow::bail!("unexpected output: nested too deeply");
        }
        let Some(c) = self.ws() else { return self.fail("value") };
        match c {
            b'{' => {
                self.at += 1;
                let mut members = Vec::new();
                while let Some(key) = self.key()? {
                    let v = self.value(depth + 1)?;
                    if members.len() < KEEP_ITEMS {
                        members.push((key, v));
                    }
                }
                Ok(Val::Obj(members))
            }
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    match self.ws() {
                        Some(b']') => {
                            self.at += 1;
                            break;
                        }
                        Some(b',') => self.at += 1,
                        Some(_) if items.len() >= KEEP_ITEMS => self.skip()?,
                        Some(_) => items.push(self.value(depth + 1)?),
                        None => return self.fail("array"),
                    }
                }
                Ok(Val::Arr(items))
            }
            b'"' => {
                self.at += 1;
                Ok(Val::Str(self.string(KEEP_STRING)?))
            }
            b'}' | b']' | b',' | b':' => self.fail("value"),
            _ => {
                let before = self.at;
                let v = self.scalar();
                if self.at == before {
                    return self.fail("value");
                }
                Ok(v)
            }
        }
    }

    /// Inside an object: the next member's key, with its colon read. `None`
    /// at the closing brace (which is read).
    pub(crate) fn key(&mut self) -> Result<Option<String>> {
        loop {
            match self.ws() {
                Some(b'}') => {
                    self.at += 1;
                    return Ok(None);
                }
                Some(b',') => self.at += 1,
                Some(b'"') => {
                    self.at += 1;
                    let key = self.string(256)?;
                    if self.ws() != Some(b':') {
                        return self.fail("member without a value");
                    }
                    self.at += 1;
                    return Ok(Some(key));
                }
                _ => return self.fail("object"),
            }
        }
    }

    /// Open an object or an array: true when the bracket was there.
    pub(crate) fn open(&mut self, bracket: u8) -> bool {
        if self.ws() == Some(bracket) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    /// Inside an array: whether another element follows (separators read).
    pub(crate) fn more(&mut self) -> Result<bool> {
        loop {
            match self.ws() {
                Some(b']') => {
                    self.at += 1;
                    return Ok(false);
                }
                Some(b',') => self.at += 1,
                Some(_) => return Ok(true),
                None => return self.fail("array"),
            }
        }
    }
}

// ------------------------------------------------------------ entities

/// Entities that never carry text and make up most of a drawing: read past
/// without building them.
fn never_text(entity: &str) -> bool {
    matches!(
        entity,
        "LINE"
            | "ARC"
            | "CIRCLE"
            | "ELLIPSE"
            | "POINT"
            | "LWPOLYLINE"
            | "POLYLINE_2D"
            | "POLYLINE_3D"
            | "POLYLINE_MESH"
            | "POLYLINE_PFACE"
            | "VERTEX_2D"
            | "VERTEX_3D"
            | "VERTEX_MESH"
            | "VERTEX_PFACE"
            | "VERTEX_PFACE_FACE"
            | "SEQEND"
            | "SPLINE"
            | "HATCH"
            | "SOLID"
            | "TRACE"
            | "_3DFACE"
            | "3DFACE"
            | "_3DSOLID"
            | "3DSOLID"
            | "REGION"
            | "BODY"
            | "RAY"
            | "XLINE"
            | "IMAGE"
            | "WIPEOUT"
            | "OLE2FRAME"
            | "OLEFRAME"
            | "VIEWPORT"
            | "MLINE"
            | "LEADER"
            | "SHAPE"
            | "MESH"
            | "HELIX"
            | "SURFACE"
            | "LIGHT"
            | "CAMERA"
            // Feature control frames: symbol-font codes, not words.
            | "TOLERANCE"
    )
}

/// Where an entity is drawn, as far as its own record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Model,
    /// Paper space, or not known.
    Elsewhere,
    /// In whatever its owner is in: the model-space block, another block,
    /// or (an attribute) its insert.
    Owner(u64),
}

struct Found {
    text: String,
    at: Option<(f64, f64)>,
    place: Place,
}

/// What the stream gave. Places are settled at the end, because an owner
/// may come later in the stream than what it owns.
#[derive(Default)]
pub(crate) struct Drawing {
    found: Vec<Found>,
    stated: Option<Extents>,
    entities: usize,
    /// Everything in the `OBJECTS` array, drawn or not. A real drawing has
    /// dozens before its first entity (layers, styles, block records).
    objects: usize,
    /// Handles of the model-space block.
    model_blocks: HashSet<u64>,
    /// Every insert and where it is placed (attributes are owned by theirs).
    inserts: HashMap<u64, Place>,
    /// The line limit was reached and reading stopped there.
    enough: bool,
}

/// String members named like text, anywhere inside an entity (multileader
/// context, table cells), in file order.
fn text_members<'a>(v: &'a Val, depth: usize, out: &mut Vec<&'a str>) {
    if depth > 12 {
        return;
    }
    match v {
        Val::Obj(m) => {
            for (k, v) in m {
                match v {
                    Val::Str(s) if matches!(k.as_str(), "text_value" | "text" | "default_text" | "user_text" | "text_string") => out.push(s),
                    Val::Obj(_) | Val::Arr(_) => text_members(v, depth + 1, out),
                    _ => {}
                }
            }
        }
        Val::Arr(a) => a.iter().for_each(|v| text_members(v, depth + 1, out)),
        _ => {}
    }
}

impl Drawing {
    fn header(&mut self, h: &Val) {
        if let (Some(lo), Some(hi)) = (h.point("EXTMIN"), h.point("EXTMAX")) {
            self.stated = Extents::stated(lo, hi);
        }
        if let Some(model) = h.handle("BLOCK_RECORD_MSPACE").filter(|h| *h != 0) {
            self.model_blocks.insert(model);
        }
    }

    /// The record of a block (not drawn): the model-space one owns model space.
    fn block_header(&mut self, o: &Val) {
        if o.str("name").is_some_and(|n| n.eq_ignore_ascii_case("*MODEL_SPACE"))
            && let Some(h) = o.handle("handle")
        {
            self.model_blocks.insert(h);
        }
    }

    fn entity(&mut self, name: &str, e: &Val, limit: usize) {
        self.entities += 1;
        // entmode 2 is model space and 1 paper space; 0 names the owner.
        let place = match (e.num("entmode").map(|m| m as i64), e.handle("ownerhandle")) {
            (Some(2), _) => Place::Model,
            (Some(0), Some(owner)) => Place::Owner(owner),
            _ => Place::Elsewhere,
        };
        if matches!(name, "INSERT" | "MINSERT") {
            if let Some(h) = e.handle("handle") {
                self.inserts.insert(h, place);
            }
            return;
        }
        let (texts, at): (Vec<String>, Option<(f64, f64)>) = match name {
            "TEXT" | "ATTRIB" => (e.str("text_value").map(plain_text).into_iter().collect(), e.point("ins_pt")),
            "ATTDEF" => {
                let default = e.str("default_value").filter(|t| !t.trim().is_empty());
                (default.or(e.str("tag")).map(plain_text).into_iter().collect(), e.point("ins_pt"))
            }
            "MTEXT" => (e.str("text").map(mtext_lines).unwrap_or_default(), e.point("ins_pt")),
            // `<>` stands for the measured value, which is not text of the file.
            n if n.starts_with("DIMENSION") => (e.str("user_text").map(|t| mtext_lines(&t.replace("<>", " "))).unwrap_or_default(), e.point("text_midpt")),
            _ => {
                let mut members = Vec::new();
                text_members(e, 0, &mut members);
                (members.into_iter().flat_map(mtext_lines).collect(), None)
            }
        };
        for text in texts {
            if text.trim().is_empty() || self.found.len() >= limit {
                continue;
            }
            self.found.push(Found { text, at, place });
        }
    }

    fn in_model(&self, place: Place) -> bool {
        match place {
            Place::Model => true,
            Place::Elsewhere => false,
            Place::Owner(h) => {
                self.model_blocks.contains(&h)
                    || match self.inserts.get(&h) {
                        Some(Place::Model) => true,
                        Some(Place::Owner(block)) => self.model_blocks.contains(block),
                        _ => false,
                    }
            }
        }
    }

    /// Hand the lines to the collector. Text in model space is placed
    /// against the stated extents, or the union of the text points when the
    /// header states none.
    pub(crate) fn emit(&self, out: &mut Out) {
        let placed: Vec<Option<(f64, f64)>> = self.found.iter().map(|f| f.at.filter(|_| self.in_model(f.place))).collect();
        let extents = self.stated.unwrap_or_else(|| {
            let mut union = Extents::empty();
            placed.iter().flatten().for_each(|p| union.add(*p));
            union
        });
        for (f, at) in self.found.iter().zip(placed) {
            let bbox = at.and_then(|p| extents.point(p)).unwrap_or_default();
            if !out.line_at(&f.text, bbox) {
                break;
            }
        }
    }
}

/// Read dwgread's JSON from `src`. Returns what was read and, when the
/// stream did not hold a whole document, why.
pub(crate) fn read_json(src: impl Read, limit: usize, stop: &dyn Fn() -> bool) -> (Drawing, Option<String>) {
    let mut d = Drawing::default();
    let mut json = Json::new(src);
    let result = (|| -> Result<()> {
        if !json.open(b'{') {
            return json.fail("not a JSON object");
        }
        while let Some(key) = json.key()? {
            match key.as_str() {
                "HEADER" => {
                    let h = json.value(0)?;
                    d.header(&h);
                }
                "OBJECTS" => {
                    if !json.open(b'[') {
                        json.skip()?;
                        continue;
                    }
                    let mut n = 0u32;
                    while json.more()? {
                        n = n.wrapping_add(1);
                        d.objects += 1;
                        if n.is_multiple_of(256) && stop() {
                            anyhow::bail!("timed out");
                        }
                        if d.found.len() >= limit {
                            // Enough: the rest of the stream is not needed.
                            d.enough = true;
                            return Ok(());
                        }
                        if !json.open(b'{') {
                            json.skip()?;
                            continue;
                        }
                        // The first member names the entity (or a non-drawn
                        // object, which is read past).
                        match json.key()? {
                            Some(k) if k == "entity" => {
                                let name = match json.value(1)? {
                                    Val::Str(s) => s,
                                    _ => String::new(),
                                };
                                if never_text(&name) {
                                    json.close_object()?;
                                    continue;
                                }
                                let mut members = Vec::new();
                                while let Some(k) = json.key()? {
                                    let v = json.value(1)?;
                                    if members.len() < KEEP_ITEMS {
                                        members.push((k, v));
                                    }
                                }
                                d.entity(&name, &Val::Obj(members), limit);
                            }
                            Some(k) if k == "object" => {
                                if matches!(json.value(1)?, Val::Str(s) if s == "BLOCK_HEADER") {
                                    let mut members = Vec::new();
                                    while let Some(k) = json.key()? {
                                        // Only its name and handle matter.
                                        if k == "name" || k == "handle" {
                                            members.push((k, json.value(1)?));
                                        } else {
                                            json.skip()?;
                                        }
                                    }
                                    d.block_header(&Val::Obj(members));
                                } else {
                                    json.close_object()?;
                                }
                            }
                            Some(_) => {
                                json.skip()?;
                                json.close_object()?;
                            }
                            None => {}
                        }
                    }
                }
                _ => json.skip()?,
            }
        }
        Ok(())
    })();
    let why = result.err().map(|e| e.to_string());
    (d, why)
}

impl<R: Read> Json<R> {
    /// Read past the remaining members of the object we are inside.
    fn close_object(&mut self) -> Result<()> {
        while self.key()?.is_some() {
            self.skip()?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------ the process

/// DWG version from the file's first six bytes, as AutoCAD names it.
fn version(magic: &[u8]) -> Option<&'static str> {
    Some(match magic.get(..6)? {
        b"AC1032" => "2018",
        b"AC1027" => "2013",
        b"AC1024" => "2010",
        b"AC1021" => "2007",
        b"AC1018" => "2004",
        b"AC1015" => "2000",
        b"AC1014" => "R14",
        b"AC1012" | b"AC1013" => "R13",
        b"AC1009" | b"AC1010" | b"AC1011" => "R11/R12",
        b"AC1006" => "R10",
        b"AC1004" | b"AC1005" => "R9",
        b"AC1002" | b"AC1003" => "R2.6",
        b"AC1001" => "R2.2",
        b"AC2.10" | b"AC2.21" | b"AC2.22" | b"AC1.50" | b"AC1.40" | b"AC1.2\0" | b"MC0.0\0" => "before R2.2",
        _ => return None,
    })
}

/// Whether the first bytes are a DWG version tag newer than `AC1032`
/// (AutoCAD 2018 to 2026), the newest LibreDWG reads.
fn newer_than_supported(magic: &[u8]) -> bool {
    match magic.get(..6) {
        Some(m) if m.starts_with(b"AC1") && m[3..].iter().all(u8::is_ascii_digit) => m > &b"AC1032"[..],
        _ => false,
    }
}

/// What a line of dwgread's messages says went wrong, in plain words.
/// At `-v0` it prints `ERROR 0x…`, the sum of LibreDWG's error flags.
fn error_said(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("ERROR")?.trim();
    let Some(code) = rest.strip_prefix("0x").and_then(|h| u32::from_str_radix(h, 16).ok()) else {
        return Some(rest.chars().take(200).collect());
    };
    let what = if code & 0x2000 != 0 {
        "out of memory"
    } else if code & 0x1000 != 0 {
        "the file could not be read"
    } else if code & 0x800 != 0 {
        "damaged or not a valid DWG"
    } else if code & 0x380 != 0 {
        "a section of the drawing is missing or damaged"
    } else if code & 0x400 != 0 {
        "an internal error in LibreDWG"
    } else {
        "errors in the drawing's objects"
    };
    Some(format!("{what} (LibreDWG error 0x{code:x})"))
}

/// A path dwgread can open. It takes its argument as 8-bit text in the
/// system code page, so a name with characters outside ASCII is passed in
/// its short (8.3) form where the volume keeps one, and as it is where the
/// code page can spell it. Anything else LibreDWG cannot be given.
#[cfg(windows)]
fn child_path(path: &Path) -> Result<std::ffi::OsString> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;
    use windows::core::PCWSTR;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn WideCharToMultiByte(code_page: u32, flags: u32, wide: *const u16, wide_len: i32, out: *mut u8, out_len: i32, default_char: *const u8, used_default: *mut i32) -> i32;
    }
    const CP_ACP: u32 = 0;
    const WC_NO_BEST_FIT_CHARS: u32 = 0x400;

    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.iter().all(|c| *c < 0x80) {
        return Ok(path.as_os_str().to_os_string());
    }
    let terminated: Vec<u16> = wide.iter().copied().chain(Some(0)).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: the path is NUL-terminated and `buf` is as long as stated.
    let n = unsafe { GetShortPathNameW(PCWSTR(terminated.as_ptr()), Some(&mut buf)) } as usize;
    if n > 0 && n < buf.len() && buf[..n].iter().all(|c| *c < 0x80) {
        return Ok(std::ffi::OsString::from_wide(&buf[..n]));
    }
    let mut bytes = vec![0u8; wide.len() * 4 + 4];
    let mut lossy = 0i32;
    // SAFETY: both buffers are valid for the lengths given, and `lossy`
    // outlives the call.
    let written = unsafe { WideCharToMultiByte(CP_ACP, WC_NO_BEST_FIT_CHARS, wide.as_ptr(), wide.len() as i32, bytes.as_mut_ptr(), bytes.len() as i32, std::ptr::null(), &mut lossy) };
    if written > 0 && lossy == 0 {
        return Ok(path.as_os_str().to_os_string());
    }
    anyhow::bail!("LibreDWG cannot open this file: its name has characters outside this PC's code page")
}

#[cfg(not(windows))]
fn child_path(path: &Path) -> Result<std::ffi::OsString> {
    Ok(path.as_os_str().to_os_string())
}

/// The command line: `dwgread -v0 -O minJSON <file>`, JSON on standard
/// output, so nothing is written to disk.
fn command(dwgread: &Path, file: &Path) -> Result<Command> {
    let mut c = Command::new(dwgread);
    c.arg("-v0").arg("-O").arg("minJSON").arg(child_path(file)?);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    Ok(c)
}

pub fn read(path: &Path, dwgread: &Path, out: &mut Out) -> Result<()> {
    let magic = crate::head(path, 6).with_context(|| "cannot open the file")?;
    if newer_than_supported(&magic) {
        anyhow::bail!("newer DWG version than LibreDWG reads ({})", String::from_utf8_lossy(&magic));
    }
    if version(&magic).is_none() {
        anyhow::bail!("damaged or not an AutoCAD drawing: it does not begin with a DWG version");
    }
    let deadline = out.deadline();
    let mut child = command(dwgread, path)?.spawn().map_err(|e| anyhow::anyhow!("LibreDWG could not be started: {e}"))?;
    let stdout = child.stdout.take().context("LibreDWG gave no output stream")?;
    let mut stderr = child.stderr.take().context("LibreDWG gave no error stream")?;
    // Its messages are drained so it never blocks on them; the start is kept.
    let messages = std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            if kept.len() < 16 * 1024 {
                kept.extend_from_slice(&buf[..n]);
            }
        }
        String::from_utf8_lossy(&kept).into_owned()
    });
    // The watchdog owns the process: it kills it at the deadline, or when
    // the reader has all it wants.
    let timed_out = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let (flag, finished) = (timed_out.clone(), done.clone());
    let watchdog = std::thread::spawn(move || {
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            let late = Instant::now() >= deadline;
            if late || finished.load(Ordering::Relaxed) {
                if late {
                    flag.store(true, Ordering::Relaxed);
                }
                let _ = child.kill();
                return child.wait().ok();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let limit = out.room().saturating_add(1);
    let (drawing, why) = read_json(stdout, limit, &|| timed_out.load(Ordering::Relaxed));
    // Stopping at the line limit is a complete reading as far as it goes.
    let complete = why.is_none();
    let enough = drawing.enough;
    done.store(true, Ordering::Relaxed);
    let status = watchdog.join().ok().flatten();
    let messages = messages.join().unwrap_or_default();
    if timed_out.load(Ordering::Relaxed) && !complete {
        anyhow::bail!("timed out");
    }
    let failed = !enough && (!complete || !status.is_some_and(|s| s.success()));
    let dwg_version = version(&magic).unwrap_or("unknown");
    if failed && drawing.found.is_empty() {
        let said = messages.lines().find_map(error_said).unwrap_or_default();
        anyhow::bail!("LibreDWG could not read this drawing (version {dwg_version}){}{said}", if said.is_empty() { "" } else { ": " });
    }
    // LibreDWG reports success for a file that has the first bytes of a
    // drawing and nothing behind them. An empty reading is not a reading.
    if !enough && drawing.objects == 0 {
        anyhow::bail!("LibreDWG read nothing from this drawing (version {dwg_version}): it is damaged, or in a form LibreDWG cannot read");
    }
    out.page("model", "");
    drawing.emit(out);
    if !complete {
        out.note("LibreDWG stopped before the end of the drawing; text may be missing");
    } else if failed {
        out.note("LibreDWG reported errors in this drawing; text may be missing");
    }
    tracing::debug!(entities = drawing.entities, lines = drawing.found.len(), "dwg read");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Val {
        Json::new(s.as_bytes()).value(0).unwrap()
    }

    #[test]
    fn json_values() {
        let v = parse(r#"{"a": [1, -2.5e1, nan, true], "s": "x\"y\\zØ😀\n", "o": {"k": null}}"#);
        assert_eq!(v.get("a"), Some(&Val::Arr(vec![Val::Num(1.0), Val::Num(-25.0), Val::Other, Val::Other])));
        assert_eq!(v.str("s"), Some("x\"y\\z\u{d8}\u{1f600}\n"));
        assert_eq!(v.get("o").and_then(|o| o.get("k")), Some(&Val::Other));
        assert_eq!(parse("[1,2,]"), Val::Arr(vec![Val::Num(1.0), Val::Num(2.0)]), "a trailing comma is accepted");
        // raw bytes that are not UTF-8 do not stop the reader
        let v = Json::new(&b"{\"t\": \"A\xD8B\"}"[..]).value(0).unwrap();
        assert_eq!(v.str("t"), Some("A\u{fffd}B"));
    }

    #[test]
    fn json_skips_without_building() {
        let mut j = Json::new(&br#"{"skip": {"deep": [1, {"x": "}]"}], "s": "\"]"}, "keep": 7}"#[..]);
        assert!(j.open(b'{'));
        assert_eq!(j.key().unwrap().as_deref(), Some("skip"));
        j.skip().unwrap();
        assert_eq!(j.key().unwrap().as_deref(), Some("keep"));
        assert_eq!(j.value(0).unwrap(), Val::Num(7.0));
        assert_eq!(j.key().unwrap(), None);
    }

    #[test]
    fn a_cut_stream_is_reported() {
        let (d, why) = read_json(&br#"{"HEADER": {}, "OBJECTS": [{"entity": "TEXT", "entmode": 2, "text_value": "ISO-1001-01", "ins_pt": [1, 2]}, {"entity": "TEXT", "text_va"#[..], 100, &|| false);
        assert_eq!(d.found.len(), 1);
        assert!(why.is_some_and(|w| w.contains("ends early")));
    }

    #[test]
    fn entities_to_lines() {
        let json = r#"{
          "created_by": "test",
          "HEADER": {"EXTMIN": [0.0, 0.0, 0.0], "EXTMAX": [400.0, 200.0, 0.0], "BLOCK_RECORD_MSPACE": [5, 1, 31, 31]},
          "CLASSES": [{"number": 500}],
          "OBJECTS": [
            {"object": "LAYER", "name": "0", "text": "not drawn"},
            {"object": "BLOCK_HEADER", "handle": [0, 1, 31], "name": "*Model_Space"},
            {"object": "BLOCK_HEADER", "handle": [0, 1, 70], "name": "TITLE"},
            {"entity": "LINE", "entmode": 2, "start": [0, 0, 0], "end": [1, 1, 0]},
            {"entity": "TEXT", "handle": [0, 1, 44], "entmode": 2, "ins_pt": [100.0, 150.0], "text_value": "PSV-1203 %%c50"},
            {"entity": "MTEXT", "entmode": 0, "ownerhandle": [4, 1, 31, 31], "ins_pt": [200.0, 100.0, 0.0], "text": "{\\fArial|b0;NOTES}\\PLINE 6\"-P-1001-A1A"},
            {"entity": "ATTRIB", "entmode": 0, "ownerhandle": [4, 1, 51, 51], "ins_pt": [300.0, 50.0], "tag": "DWGNO", "text_value": "ISO-1001-01"},
            {"entity": "INSERT", "handle": [0, 1, 51], "entmode": 2, "ins_pt": [350.0, 10.0, 0.0], "has_attribs": 1},
            {"entity": "SEQEND", "entmode": 0},
            {"entity": "TEXT", "entmode": 1, "ins_pt": [100.0, 150.0], "text_value": "PAPER SPACE NOTE"},
            {"entity": "DIMENSION_LINEAR", "entmode": 2, "text_midpt": [40.0, 20.0], "user_text": "<> TYP"},
            {"entity": "DIMENSION_LINEAR", "entmode": 2, "text_midpt": [40.0, 20.0], "user_text": ""},
            {"entity": "ATTDEF", "entmode": 0, "ownerhandle": [4, 1, 70, 70], "ins_pt": [1.0, 1.0], "tag": "DWGNO", "default_value": "", "prompt": "Drawing number"},
            {"entity": "TEXT", "entmode": 0, "ownerhandle": [4, 1, 70, 70], "ins_pt": [2.0, 2.0], "text_value": "IN A BLOCK"},
            {"entity": "MULTILEADER", "entmode": 2, "ctx": {"content": {"txt": {"default_text": "\\LSEE DETAIL A"}}}}
          ]
        }"#;
        let (d, why) = read_json(json.as_bytes(), 100, &|| false);
        assert_eq!(why, None);
        let got: Vec<(&str, bool)> = d.found.iter().map(|f| (f.text.as_str(), f.at.is_some() && d.in_model(f.place))).collect();
        assert_eq!(
            got,
            [
                ("PSV-1203 \u{d8}50", true),
                ("NOTES", true),
                ("LINE 6\"-P-1001-A1A", true),
                // its insert comes later in the stream; still placed
                ("ISO-1001-01", true),
                ("PAPER SPACE NOTE", false),
                ("TYP", true),
                ("DWGNO", false),
                ("IN A BLOCK", false),
                ("SEE DETAIL A", false),
            ]
        );
        assert_eq!(d.stated, Extents::stated((0.0, 0.0), (400.0, 200.0)));
        assert_eq!(d.entities, 10, "the line is read past without being built");
    }

    #[test]
    fn versions() {
        assert_eq!(version(b"AC1032\0\0"), Some("2018"));
        assert_eq!(version(b"AC1015"), Some("2000"));
        assert_eq!(version(b"PK\x03\x04\0\0"), None);
        assert!(newer_than_supported(b"AC1035"));
        assert!(!newer_than_supported(b"AC1032"));
        assert!(!newer_than_supported(b"AC1.50"));
        assert_eq!(error_said("ERROR 0x800").as_deref(), Some("damaged or not a valid DWG (LibreDWG error 0x800)"));
        assert_eq!(error_said(" ERROR 0x1140 ").as_deref(), Some("the file could not be read (LibreDWG error 0x1140)"));
        assert_eq!(error_said("SUCCESS"), None);
    }
}
