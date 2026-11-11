//! Email: Outlook `.msg` (a compound file of MAPI properties) and `.eml`
//! (RFC 822 text). Both give the same page: Subject, From, To, Cc, Date,
//! the body as lines, then one `Attachment: name` line per attachment.
//!
//! Attachments are named, not opened. A `.msg` body kept only as compressed
//! RTF is not read; the page note says so.

use crate::Out;
use anyhow::{Context, Result};
use encoding_rs::{Encoding, UTF_8, UTF_16LE, WINDOWS_1252};
use std::io::Read;
use std::path::Path;

// ---------------------------------------------------------------- .msg

type Cf = cfb::CompoundFile<std::fs::File>;

fn stream(cf: &mut Cf, path: &str) -> Option<Vec<u8>> {
    let mut s = cf.open_stream(path).ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// A Windows code page number as an encoding (Windows-1252 when unknown).
fn code_page(cp: u32) -> &'static Encoding {
    let label = match cp {
        65001 => "utf-8",
        20127 | 1252 => "windows-1252",
        28591 => "iso-8859-1",
        28592 => "iso-8859-2",
        28595 => "iso-8859-5",
        28597 => "iso-8859-7",
        28599 => "iso-8859-9",
        28605 => "iso-8859-15",
        874 => "windows-874",
        932 => "shift_jis",
        936 => "gbk",
        949 => "euc-kr",
        950 => "big5",
        1250 => "windows-1250",
        1251 => "windows-1251",
        1253 => "windows-1253",
        1254 => "windows-1254",
        1255 => "windows-1255",
        1256 => "windows-1256",
        1257 => "windows-1257",
        1258 => "windows-1258",
        20866 => "koi8-r",
        50220 => "iso-2022-jp",
        51932 => "euc-jp",
        54936 => "gb18030",
        _ => "windows-1252",
    };
    Encoding::for_label(label.as_bytes()).unwrap_or(WINDOWS_1252)
}

/// A string property of the storage at `dir`: Unicode (type 001F) first,
/// then the 8-bit form (001E) in the message's code page.
fn prop_str(cf: &mut Cf, dir: &str, id: u16, cp: &'static Encoding) -> Option<String> {
    let text = if let Some(b) = stream(cf, &format!("{dir}/__substg1.0_{id:04X}001F")) {
        UTF_16LE.decode_without_bom_handling(&b).0.into_owned()
    } else {
        let b = stream(cf, &format!("{dir}/__substg1.0_{id:04X}001E"))?;
        cp.decode_without_bom_handling(&b).0.into_owned()
    };
    let text = text.trim_matches(['\0', ' ', '\r', '\n', '\t']).to_string();
    (!text.is_empty()).then_some(text)
}

/// The fixed-size properties of a message: (id, type) → the 8 value bytes.
fn fixed_props(cf: &mut Cf) -> Vec<(u16, u16, [u8; 8])> {
    let Some(b) = stream(cf, "/__properties_version1.0") else { return Vec::new() };
    // A top-level message has a 32-byte header before the 16-byte entries.
    b.get(32..)
        .unwrap_or_default()
        .as_chunks::<16>()
        .0
        .iter()
        .map(|e| {
            let mut v = [0u8; 8];
            v.copy_from_slice(&e[8..16]);
            (u16::from_le_bytes([e[2], e[3]]), u16::from_le_bytes([e[0], e[1]]), v)
        })
        .collect()
}

/// Days since 1970-01-01 as a calendar date.
pub(crate) fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A FILETIME (100 ns ticks since 1601, UTC) as `2026-03-14 10:30:00 UTC`.
fn filetime(ticks: u64) -> Option<String> {
    if ticks == 0 || ticks > i64::MAX as u64 {
        return None;
    }
    let secs = (ticks / 10_000_000) as i64 - 11_644_473_600;
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    Some(format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", t / 3600, t / 60 % 60, t % 60))
}

pub fn read_msg(path: &Path, out: &mut Out) -> Result<()> {
    let mut cf = cfb::open(path).map_err(|e| anyhow::anyhow!("damaged: not a readable Outlook message ({e})"))?;
    if !cf.is_stream("/__properties_version1.0") {
        anyhow::bail!("damaged or not an Outlook message: it has no message properties");
    }
    let fixed = fixed_props(&mut cf);
    let long = |id: u16| fixed.iter().find(|(i, t, _)| *i == id && *t == 0x0003).map(|(_, _, v)| u32::from_le_bytes([v[0], v[1], v[2], v[3]]));
    let time = |id: u16| fixed.iter().find(|(i, t, _)| *i == id && *t == 0x0040).and_then(|(_, _, v)| filetime(u64::from_le_bytes(*v)));
    // PR_MESSAGE_CODEPAGE says how the 8-bit strings are written;
    // PR_INTERNET_CPID how the HTML body is.
    let cp = code_page(long(0x3FFD).unwrap_or(1252));
    let body_cp = long(0x3FDE).map(code_page);

    out.page("email", "");
    if let Some(s) = prop_str(&mut cf, "", 0x0037, cp) {
        out.line(&format!("Subject: {s}"));
    }
    let name = prop_str(&mut cf, "", 0x0C1A, cp).or_else(|| prop_str(&mut cf, "", 0x0042, cp));
    // The SMTP address where the message has one; an Exchange directory
    // name (no `@`) is not an address anyone would search for.
    let addr = [0x5D01u16, 0x5D02, 0x0C1F, 0x0065].into_iter().filter_map(|id| prop_str(&mut cf, "", id, cp)).find(|a| a.contains('@'));
    match (name, addr) {
        (Some(n), Some(a)) if n != a => out.line(&format!("From: {n} <{a}>")),
        (Some(n), _) => out.line(&format!("From: {n}")),
        (None, Some(a)) => out.line(&format!("From: {a}")),
        (None, None) => true,
    };
    if let Some(s) = prop_str(&mut cf, "", 0x0E04, cp) {
        out.line(&format!("To: {s}"));
    }
    if let Some(s) = prop_str(&mut cf, "", 0x0E03, cp) {
        out.line(&format!("Cc: {s}"));
    }
    // Sent time; a received message that lost it still has a delivery time.
    if let Some(s) = time(0x0039).or_else(|| time(0x0E06)) {
        out.line(&format!("Date: {s}"));
    }

    if let Some(body) = prop_str(&mut cf, "", 0x1000, cp) {
        for l in body.split(['\n', '\r']) {
            out.tick()?;
            if !out.line(l) {
                break;
            }
        }
    } else if let Some(html) = html_body(&mut cf, body_cp) {
        for l in html_lines(&html) {
            out.tick()?;
            if !out.line(&l) {
                break;
            }
        }
    } else if cf.is_stream("/__substg1.0_10090102") {
        out.note("the body is compressed RTF and was not read");
    }

    let attachments: Vec<String> = cf.read_root_storage().filter(|e| e.is_storage() && e.name().starts_with("__attach_version1.0_")).map(|e| e.name().to_string()).collect();
    for a in attachments {
        let dir = format!("/{a}");
        // Long file name, short file name, then the display name (an
        // attached message has only that).
        if let Some(n) = [0x3707u16, 0x3704, 0x3001].into_iter().find_map(|id| prop_str(&mut cf, &dir, id, cp)) {
            out.line(&format!("Attachment: {n}"));
        }
    }
    Ok(())
}

fn html_body(cf: &mut Cf, cp: Option<&'static Encoding>) -> Option<String> {
    if let Some(b) = stream(cf, "/__substg1.0_10130102") {
        let text = match (cp, std::str::from_utf8(&b)) {
            (Some(cp), _) => cp.decode_without_bom_handling(&b).0.into_owned(),
            (None, Ok(s)) => s.to_string(),
            (None, Err(_)) => WINDOWS_1252.decode_without_bom_handling(&b).0.into_owned(),
        };
        return (!text.trim().is_empty()).then_some(text);
    }
    let b = stream(cf, "/__substg1.0_1013001F")?;
    let text = UTF_16LE.decode_without_bom_handling(&b).0.into_owned();
    (!text.trim().is_empty()).then_some(text)
}

// ---------------------------------------------------------------- HTML

/// The text of an HTML document as lines: tags go, block elements end a
/// line, table cells are joined with ` | `, scripts and styles are skipped.
pub fn html_lines(html: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut rest = html;
    let mut skip_until: Option<&'static str> = None;
    while !rest.is_empty() {
        if let Some(close) = skip_until {
            // Inside <script> or <style>: nothing counts until it closes.
            match find_ci(rest, close) {
                Some(i) => {
                    rest = &rest[i..];
                    skip_until = None;
                }
                None => break,
            }
            continue;
        }
        let Some(lt) = rest.find('<') else {
            cur.push_str(&unescape(rest));
            break;
        };
        cur.push_str(&unescape(&rest[..lt]));
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map(|i| &rest[i + 3..]).unwrap_or("");
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        match name.as_str() {
            "script" if !closing => skip_until = Some("</script"),
            "style" if !closing => skip_until = Some("</style"),
            "td" | "th" if closing => cur.push_str(" | "),
            "br" | "p" | "div" | "tr" | "li" | "ul" | "ol" | "table" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "hr" | "blockquote" | "pre" | "title" | "head" | "body" | "html" | "dt" | "dd" | "caption" | "section" | "article" | "header" | "footer" => {
                lines.push(std::mem::take(&mut cur));
            }
            _ => {}
        }
    }
    lines.push(cur);
    lines
        .into_iter()
        .map(|l| {
            // Cells of a row: drop the empty ones and the trailing separator.
            l.split(" | ").map(crate::out::clean).filter(|c| !c.is_empty()).collect::<Vec<_>>().join(" | ")
        })
        .filter(|l| !l.is_empty())
        .collect()
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    (0..h.len().saturating_sub(n.len() - 1)).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Character references and the handful of named entities mail and report
/// HTML uses; an unknown name stays as written.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map(|i| i + 1).unwrap_or(rest.len());
        let name = &rest[1..end];
        let known = if let Some(num) = name.strip_prefix('#') {
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => num.parse().ok(),
            };
            code.and_then(char::from_u32).map(String::from)
        } else {
            let c = match name {
                "amp" => "&",
                "lt" => "<",
                "gt" => ">",
                "quot" => "\"",
                "apos" => "'",
                "nbsp" => " ",
                "ndash" => "\u{2013}",
                "mdash" => "\u{2014}",
                "deg" => "\u{b0}",
                "plusmn" => "\u{b1}",
                "micro" => "\u{b5}",
                "Oslash" => "\u{d8}",
                "oslash" => "\u{f8}",
                "copy" => "\u{a9}",
                "reg" => "\u{ae}",
                "lsquo" => "\u{2018}",
                "rsquo" => "\u{2019}",
                "ldquo" => "\u{201c}",
                "rdquo" => "\u{201d}",
                "bull" => "\u{2022}",
                "hellip" => "\u{2026}",
                "euro" => "\u{20ac}",
                "pound" => "\u{a3}",
                "frac12" => "\u{bd}",
                "frac14" => "\u{bc}",
                "frac34" => "\u{be}",
                "times" => "\u{d7}",
                _ => "",
            };
            (!c.is_empty()).then(|| c.to_string())
        };
        match known {
            Some(c) => {
                out.push_str(&c);
                rest = &rest[end..];
                rest = rest.strip_prefix(';').unwrap_or(rest);
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------- .eml

/// 8-bit text with no declared charset: UTF-8 when valid, else Windows-1252.
fn loose_text(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => WINDOWS_1252.decode_without_bom_handling(b).0.into_owned(),
    }
}

fn charset_text(b: &[u8], charset: Option<&str>) -> String {
    match charset.and_then(|c| Encoding::for_label(c.trim().as_bytes())) {
        Some(e) if e == UTF_8 => String::from_utf8_lossy(b).into_owned(),
        Some(e) => e.decode_without_bom_handling(b).0.into_owned(),
        None => loose_text(b),
    }
}

pub(crate) fn base64(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in input {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Quoted-printable. `header` is the RFC 2047 variant, where `_` is a space.
pub(crate) fn quoted_printable(input: &[u8], header: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'=' => {
                if let (Some(h), Some(l)) = (input.get(i + 1).copied().and_then(hex), input.get(i + 2).copied().and_then(hex)) {
                    out.push(h << 4 | l);
                    i += 3;
                } else if input.get(i + 1) == Some(&b'\n') {
                    i += 2; // soft line break
                } else if input.get(i + 1) == Some(&b'\r') {
                    i += if input.get(i + 2) == Some(&b'\n') { 3 } else { 2 };
                } else {
                    out.push(b'=');
                    i += 1;
                }
            }
            b'_' if header => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// A header value with its RFC 2047 encoded words (`=?utf-8?B?…?=`) decoded.
/// White space between two encoded words is not part of the text.
pub(crate) fn decode_words(raw: &[u8]) -> String {
    let s = loose_text(raw);
    let mut out = String::new();
    let mut rest = s.as_str();
    let mut last_was_word = false;
    while let Some(start) = rest.find("=?") {
        let word = (|| {
            let body = &rest[start + 2..];
            let (charset, body) = body.split_once('?')?;
            let (enc, body) = body.split_once('?')?;
            let (text, _) = body.split_once("?=")?;
            if charset.is_empty() || charset.contains(char::is_whitespace) || text.contains(char::is_whitespace) {
                return None;
            }
            let len = 2 + charset.len() + 1 + enc.len() + 1 + text.len() + 2;
            let bytes = match enc {
                "B" | "b" => base64(text.as_bytes()),
                "Q" | "q" => quoted_printable(text.as_bytes(), true),
                _ => return None,
            };
            // `charset*lang` (RFC 2231) names a language too.
            let charset = charset.split('*').next().unwrap_or(charset);
            Some((charset_text(&bytes, Some(charset)), start + len))
        })();
        match word {
            Some((text, end)) => {
                let before = &rest[..start];
                if !(last_was_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&text);
                rest = &rest[end..];
                last_was_word = true;
            }
            None => {
                out.push_str(&rest[..start + 2]);
                rest = &rest[start + 2..];
                last_was_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

struct Headers(Vec<(String, Vec<u8>)>);

impl Headers {
    fn get(&self, name: &str) -> Option<&[u8]> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_slice())
    }

    fn text(&self, name: &str) -> Option<String> {
        let t = decode_words(self.get(name)?);
        let t = crate::out::clean(&t);
        (!t.is_empty()).then_some(t)
    }
}

/// Split a message (or a MIME part) into its headers and its body.
/// Continuation lines are joined to the header they continue.
fn split_message(b: &[u8]) -> (Headers, &[u8]) {
    let mut headers: Vec<(String, Vec<u8>)> = Vec::new();
    let mut pos = 0;
    while pos < b.len() {
        let end = b[pos..].iter().position(|c| *c == b'\n').map(|i| pos + i).unwrap_or(b.len());
        let line = b[pos..end].strip_suffix(b"\r").unwrap_or(&b[pos..end]);
        let next = (end + 1).min(b.len());
        if line.is_empty() {
            pos = next;
            break;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            if let Some((_, v)) = headers.last_mut() {
                v.push(b' ');
                v.extend_from_slice(line.trim_ascii());
            }
        } else if let Some(colon) = line.iter().position(|c| *c == b':') {
            headers.push((String::from_utf8_lossy(&line[..colon]).trim().to_ascii_lowercase(), line[colon + 1..].trim_ascii().to_vec()));
        } else if headers.is_empty() {
            // No header at all: the whole thing is body (an mbox "From " line
            // is the one exception, and it is skipped).
            if !line.starts_with(b"From ") {
                return (Headers(headers), b);
            }
        }
        pos = next;
    }
    (Headers(headers), &b[pos.min(b.len())..])
}

/// A structured header value (`text/plain; charset="utf-8"; name=x.pdf`):
/// the value in lower case and its parameters, RFC 2231 forms included.
fn params(raw: &[u8]) -> (String, Vec<(String, String)>) {
    let s = loose_text(raw);
    let mut parts = Vec::new();
    let (mut cur, mut quoted) = (String::new(), false);
    for c in s.chars() {
        match c {
            ';' if !quoted => parts.push(std::mem::take(&mut cur)),
            c => {
                quoted ^= c == '"';
                cur.push(c);
            }
        }
    }
    parts.push(cur);
    let mut it = parts.into_iter();
    let value = it.next().unwrap_or_default().trim().to_ascii_lowercase();
    let mut out: Vec<(String, String)> = Vec::new();
    for p in it {
        let Some((k, v)) = p.split_once('=') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        // Quotes keep the spaces inside them.
        let v = v.strip_prefix('"').map(|v| v.strip_suffix('"').unwrap_or(v)).unwrap_or(v);
        // `name*=utf-8''a%20b.pdf`, and `name*0=`, `name*1=` continuations.
        let (key, extended) = match k.split_once('*') {
            Some((base, tail)) => (base.to_string(), tail.is_empty() || tail.ends_with('*')),
            None => (k.clone(), false),
        };
        let v = if extended { percent(v) } else { decode_words(v.as_bytes()) };
        match out.iter_mut().find(|(n, _)| *n == key) {
            Some((_, old)) if k.contains('*') => old.push_str(&v),
            Some(_) => {}
            None => out.push((key, v)),
        }
    }
    (value, out)
}

/// `utf-8'en'a%20b.pdf` → `a b.pdf`.
fn percent(v: &str) -> String {
    let (charset, text) = match v.split_once('\'') {
        Some((cs, rest)) => (Some(cs), rest.split_once('\'').map(|(_, t)| t).unwrap_or(rest)),
        None => (None, v),
    };
    let b = text.as_bytes();
    let mut bytes = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex))
        {
            bytes.push(h << 4 | l);
            i += 3;
        } else {
            bytes.push(b[i]);
            i += 1;
        }
    }
    charset_text(&bytes, charset.filter(|c| !c.is_empty()))
}

fn param<'a>(ps: &'a [(String, String)], name: &str) -> Option<&'a str> {
    ps.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

#[derive(Default)]
struct Found {
    plain: Option<String>,
    html: Option<String>,
    attachments: Vec<String>,
}

/// Walk a MIME tree: the first text/plain and text/html parts that are not
/// attachments, and the name of every attachment.
fn mime(headers: &Headers, body: &[u8], depth: u32, found: &mut Found) {
    let (ctype, cparams) = headers.get("content-type").map(params).unwrap_or_else(|| ("text/plain".into(), Vec::new()));
    let (disp, dparams) = headers.get("content-disposition").map(params).unwrap_or_default();
    let name = param(&dparams, "filename").or_else(|| param(&cparams, "name")).map(crate::out::clean).filter(|n| !n.is_empty());
    if ctype.starts_with("multipart/") {
        let Some(boundary) = param(&cparams, "boundary").filter(|b| !b.is_empty()) else { return };
        if depth >= 12 {
            return;
        }
        for part in split_parts(body, boundary.as_bytes()) {
            let (h, b) = split_message(part);
            mime(&h, b, depth + 1, found);
        }
        return;
    }
    if disp == "attachment" || (name.is_some() && !ctype.starts_with("text/")) {
        found.attachments.push(name.unwrap_or_else(|| ctype.clone()));
        return;
    }
    if ctype == "message/rfc822" {
        found.attachments.push(name.unwrap_or_else(|| "attached message".into()));
        return;
    }
    let wanted = (ctype == "text/plain" && found.plain.is_none()) || (ctype == "text/html" && found.html.is_none());
    if !wanted {
        return;
    }
    let encoding = headers.get("content-transfer-encoding").map(|v| loose_text(v).trim().to_ascii_lowercase()).unwrap_or_default();
    let bytes = match encoding.as_str() {
        "base64" => base64(body),
        "quoted-printable" => quoted_printable(body, false),
        _ => body.to_vec(),
    };
    let text = charset_text(&bytes, param(&cparams, "charset"));
    if ctype == "text/plain" { found.plain = Some(text) } else { found.html = Some(text) }
}

/// The parts between `--boundary` lines, up to the closing `--boundary--`.
fn split_parts<'a>(body: &'a [u8], boundary: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut start: Option<usize> = None;
    let mut pos = 0;
    while pos <= body.len() {
        let end = body[pos..].iter().position(|c| *c == b'\n').map(|i| pos + i).unwrap_or(body.len());
        let line = body[pos..end].strip_suffix(b"\r").unwrap_or(&body[pos..end]);
        if let Some(rest) = line.strip_prefix(b"--").and_then(|l| l.strip_prefix(boundary)) {
            let rest = rest.trim_ascii();
            if rest.is_empty() || rest == b"--" {
                if let Some(s) = start {
                    // The line break before a boundary belongs to the boundary.
                    let mut e = pos;
                    if e > s && body[e - 1] == b'\n' {
                        e -= 1;
                    }
                    if e > s && body[e - 1] == b'\r' {
                        e -= 1;
                    }
                    parts.push(&body[s..e.max(s)]);
                }
                start = (rest.is_empty()).then_some((end + 1).min(body.len()));
                if rest == b"--" {
                    break;
                }
            }
        }
        if end >= body.len() {
            break;
        }
        pos = end + 1;
    }
    // A message cut off before its closing boundary still has its last part.
    if let Some(s) = start
        && s < body.len()
    {
        parts.push(&body[s..]);
    }
    parts
}

pub fn read_eml(path: &Path, out: &mut Out) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| "cannot read the file")?;
    // Some tools save .eml as UTF-16; the headers must be bytes we can scan.
    let bytes = if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) { crate::plain::decode(&bytes)?.into_owned().into_bytes() } else { bytes };
    let (headers, body) = split_message(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes));
    if !["from", "to", "subject", "date", "received", "message-id", "mime-version", "content-type"].iter().any(|h| headers.get(h).is_some()) {
        anyhow::bail!("not an email: it has no message headers");
    }
    out.page("email", "");
    for (name, label) in [("subject", "Subject"), ("from", "From"), ("to", "To"), ("cc", "Cc"), ("date", "Date")] {
        if let Some(v) = headers.text(name) {
            out.line(&format!("{label}: {v}"));
        }
    }
    let mut found = Found::default();
    mime(&headers, body, 0, &mut found);
    if let Some(text) = found.plain.filter(|t| !t.trim().is_empty()) {
        for l in text.split(['\n', '\r']) {
            out.tick()?;
            if !out.line(l) {
                break;
            }
        }
    } else if let Some(html) = found.html {
        for l in html_lines(&html) {
            out.tick()?;
            if !out.line(&l) {
                break;
            }
        }
    }
    for a in found.attachments {
        out.line(&format!("Attachment: {a}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_words() {
        assert_eq!(decode_words(b"=?UTF-8?B?UFNWLTEyMDMg4oCTIHNldCBwcmVzc3VyZQ==?="), "PSV-1203 \u{2013} set pressure");
        assert_eq!(decode_words(b"RE: =?iso-8859-1?Q?=D850_flange?= for ISO-1001-01"), "RE: \u{d8}50 flange for ISO-1001-01");
        // two words split only to fit the line length are one text
        assert_eq!(decode_words(b"=?utf-8?Q?Line_?=\r\n =?utf-8?Q?list?="), "Line list");
        assert_eq!(decode_words(b"plain =?not a word"), "plain =?not a word");
    }

    #[test]
    fn transfer_encodings() {
        assert_eq!(base64(b"SVNPLTEw\r\nMDEtMDE="), b"ISO-1001-01");
        assert_eq!(quoted_printable(b"45=B0 at P-101A=\r\n, checked", false), b"45\xB0 at P-101A, checked");
    }

    #[test]
    fn html_to_lines() {
        let html = "<html><head><style>p { color: red }</style><title>Report</title></head><body><p>Line 6&quot;-P-1001-A1A&nbsp;to pump</p>\
                    <table><tr><td>PSV-1203</td><td></td><td>Relief valve &amp; spare</td></tr></table><script>var x = '<p>no</p>';</script>Tail<br>&#216;50</body></html>";
        assert_eq!(html_lines(html), ["Report", "Line 6\"-P-1001-A1A to pump", "PSV-1203 | Relief valve & spare", "Tail", "\u{d8}50"]);
    }

    #[test]
    fn dates_from_filetime() {
        // 2026-03-14 10:30:00 UTC
        let secs: u64 = 1_773_484_200 + 11_644_473_600;
        assert_eq!(filetime(secs * 10_000_000).as_deref(), Some("2026-03-14 10:30:00 UTC"));
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(-1), (1969, 12, 31));
        assert_eq!(civil(11_016), (2000, 2, 29));
    }

    #[test]
    fn parameters() {
        let (v, p) = params(b"Multipart/Mixed; boundary=\"=_b;1\"; charset=utf-8");
        assert_eq!(v, "multipart/mixed");
        assert_eq!(param(&p, "boundary"), Some("=_b;1"));
        let (_, p) = params(b"attachment; filename*=utf-8''ISO-1001-01%20Rev%20B.pdf");
        assert_eq!(param(&p, "filename"), Some("ISO-1001-01 Rev B.pdf"));
        let (_, p) = params(b"attachment; filename*0=\"line list \"; filename*1=\"rev 2.xlsx\"");
        assert_eq!(param(&p, "filename"), Some("line list rev 2.xlsx"));
    }
}
