//! Dexterity Connect, the office messenger.
//!
//! * Out: "Share to Dexterity Connect" on a result builds a `dexconnect://share` link that opens
//!   the messenger's share dialog with the file attached and what Sleuth found in it.
//! * In: a `sleuth://search?q=<tag>` link (line or drawing numbers in chat are links like this)
//!   opens Sleuth and runs that search.

/// The link scheme Sleuth registers for itself.
pub const SCHEME: &str = "sleuth";

/// Percent-encode everything except unreserved characters (RFC 3986).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decode `%XX` and `+`; invalid escapes are kept as they are.
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => { out.push(v); i += 3; }
                    None => { out.push(b'%'); i += 1; }
                }
            }
            b'+' => { out.push(b' '); i += 1; }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The share link for one search result.
pub fn share_url(file_path: &str, found: &str, query: &str) -> String {
    let mut text = String::from("Found with Sleuth");
    if !query.trim().is_empty() {
        text.push_str(&format!(" (\"{}\")", query.trim()));
    }
    let found = found.trim();
    if !found.is_empty() {
        let snippet: String = found.chars().take(400).collect();
        text.push_str(&format!(":\n> {}", snippet.replace('\n', "\n> ")));
    }
    let mut url = format!("dexconnect://share?from=Sleuth&text={}", encode(&text));
    if !file_path.trim().is_empty() {
        url.push_str(&format!("&file={}", encode(file_path.trim())));
    }
    url
}

/// The search asked for by a `sleuth://search?q=…` argument, if any (at most 200 characters).
pub fn search_from_args<S: AsRef<str>>(args: &[S]) -> Option<String> {
    args.iter().find_map(|a| {
        let a = a.as_ref().trim().trim_matches('"');
        let rest = a.strip_prefix("sleuth://")?;
        let rest = rest.strip_prefix("search").unwrap_or(rest).trim_start_matches('/');
        let query = rest.strip_prefix('?')?;
        let q = query.split('&').find_map(|kv| kv.strip_prefix("q="))?;
        let q: String = decode(q).chars().filter(|c| !c.is_control()).take(200).collect();
        let q = q.trim().to_string();
        (!q.is_empty()).then_some(q)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_link_carries_file_and_found_text() {
        let url = share_url(r"\\nas01\P12513\ISO-1001-01.pdf", "6\"-P-1001-A1B\nREV B", "P-1001");
        assert!(url.starts_with("dexconnect://share?from=Sleuth&text=Found%20with%20Sleuth"));
        assert!(url.contains("&file=%5C%5Cnas01%5CP12513%5CISO-1001-01.pdf"));
        assert!(!url.contains(' '));
        assert!(decode(&url).contains("> 6\"-P-1001-A1B\n> REV B"));
    }

    #[test]
    fn search_links_are_read_from_the_command_line() {
        assert_eq!(search_from_args(&["sleuth.exe", "sleuth://search?q=6%22-P-1001-A1B"]), Some("6\"-P-1001-A1B".into()));
        assert_eq!(search_from_args(&["\"sleuth://search/?q=ISO-1001-01&x=1\""]), Some("ISO-1001-01".into()));
        assert_eq!(search_from_args(&["sleuth://search?q="]), None);
        assert_eq!(search_from_args(&["--elevated"]), None);
        assert_eq!(search_from_args(&["sleuth://search?q=a%0Ab"]), Some("ab".into()));
        // bad escapes do not panic
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz%4"), "%zz%4");
    }
}
