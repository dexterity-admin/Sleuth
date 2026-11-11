//! Drive letters, UNC paths and file-server identity.
//!
//! Mapped drive letters differ from PC to PC, so everything Sleuth stores is
//! keyed by UNC path (`\\fs01\projects\...`). Paths are translated back
//! to the viewer's own drive letters only for display.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drive {
    /// `"C:"`, `"W:"`.
    pub letter: String,
    pub kind: DriveKind,
    /// `\\server\share` for mapped network drives.
    pub unc: Option<String>,
}

impl Drive {
    /// Root to walk: `C:\` for local drives, the UNC share for network drives.
    pub fn root(&self) -> String {
        match &self.unc {
            Some(u) => format!("{}\\", u.trim_end_matches('\\')),
            None => format!("{}\\", self.letter),
        }
    }
}

/// Lower-cased `\\host` part of a UNC path, or `local:<letter>` for local paths.
/// Two drive letters that map to the same host share one concurrency budget.
pub fn server_key(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("\\\\") {
        let host = rest.split('\\').next().unwrap_or("");
        format!("\\\\{}", host.to_lowercase())
    } else {
        let letter: String = path.chars().take(2).collect();
        format!("local:{}", letter.to_uppercase())
    }
}

/// Map between this PC's drive letters and UNC shares.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriveMap {
    pub drives: Vec<Drive>,
}

impl DriveMap {
    /// `W:\Isometrics\a.pdf` -> `\\fs01\projects\Isometrics\a.pdf`.
    pub fn to_unc(&self, path: &str) -> String {
        let letter: String = path.chars().take(2).collect::<String>().to_uppercase();
        for d in &self.drives {
            if let Some(unc) = &d.unc
                && d.letter == letter
                && let Some(rest) = path.get(2..)
            {
                return format!("{}{}", unc.trim_end_matches('\\'), rest);
            }
        }
        path.to_string()
    }

    /// Reverse of [`to_unc`](Self::to_unc): use this PC's drive letter when the
    /// share is mapped, otherwise keep the UNC path so the file still opens.
    pub fn to_display(&self, path: &str) -> String {
        let lower = path.to_lowercase();
        let mut best: Option<(&Drive, usize)> = None;
        for d in &self.drives {
            if let Some(unc) = &d.unc {
                let u = unc.trim_end_matches('\\').to_lowercase();
                let boundary = || lower.get(u.len()..).is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'));
                if lower.starts_with(&u) && boundary() && best.is_none_or(|(_, n)| u.len() > n) {
                    best = Some((d, u.len()));
                }
            }
        }
        match best {
            Some((d, n)) => match path.get(n..) {
                Some(rest) => format!("{}{}", d.letter, rest),
                None => path.to_string(),
            },
            None => path.to_string(),
        }
    }

    #[cfg(windows)]
    pub fn current() -> DriveMap {
        DriveMap { drives: win::list_drives() }
    }

    #[cfg(not(windows))]
    pub fn current() -> DriveMap {
        DriveMap::default()
    }

    /// Give one network path to drives that reach the same share by
    /// different names (`\\Docsrv-1\X` and `\\10.0.0.54\x`), so the
    /// share is walked once and every path to it is stored one way.
    /// `address` turns a host name into its IP address. Returns the merged
    /// `(alias, kept)` paths; the first drive letter's form is kept.
    pub fn merge_aliases(&mut self, address: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
        let mut seen: Vec<(String, String)> = Vec::new(); // (ip\share, unc)
        let mut merged = Vec::new();
        for d in &mut self.drives {
            let Some(unc) = d.unc.clone() else { continue };
            let Some(rest) = unc.trim_end_matches('\\').strip_prefix("\\\\") else { continue };
            let mut parts = rest.splitn(2, '\\');
            let (host, share) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            if host.is_empty() || share.is_empty() {
                continue;
            }
            let Some(ip) = address(host) else { continue };
            let key = format!("{}\\{}", ip, share.to_lowercase());
            match seen.iter().find(|(k, _)| *k == key) {
                Some((_, kept)) if !kept.eq_ignore_ascii_case(unc.trim_end_matches('\\')) => {
                    merged.push((unc.trim_end_matches('\\').to_string(), kept.clone()));
                    d.unc = Some(kept.clone());
                }
                Some(_) => {}
                None => seen.push((key, unc.trim_end_matches('\\').to_string())),
            }
        }
        merged
    }

    /// Write every mapped share with its server's IPv4 address
    /// (see [`canonical_path`]).
    pub fn canonicalize(&mut self, address: &dyn Fn(&str) -> Option<String>) {
        for d in &mut self.drives {
            if let Some(u) = &d.unc {
                d.unc = Some(canonical_path(u, address));
            }
        }
    }
}

/// A network path with its host as an IPv4 address
/// (`\\Docsrv-1\Docs\a.pdf` -> `\\10.0.0.54\Docs\a.pdf`), so every PC
/// writes a share the same way in the office store whatever name it mapped
/// it by. Local paths and hosts `address` cannot resolve stay as they are.
pub fn canonical_path(path: &str, address: &dyn Fn(&str) -> Option<String>) -> String {
    let Some(rest) = path.strip_prefix("\\\\") else { return path.to_string() };
    let (host, tail) = match rest.split_once('\\') {
        Some((h, t)) => (h, Some(t)),
        None => (rest, None),
    };
    if host.is_empty() {
        return path.to_string();
    }
    match (address(host), tail) {
        (Some(ip), Some(t)) => format!("\\\\{ip}\\{t}"),
        (Some(ip), None) => format!("\\\\{ip}"),
        (None, _) => path.to_string(),
    }
}

/// A root as a folder name in the office store: `10.0.0.54~docs` for
/// `\\10.0.0.54\Docs`, `local~c` for `C:\`.
pub fn root_key(root: &str) -> String {
    let safe = |s: &str| -> String { s.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c.to_ascii_lowercase() } else { '_' }).collect() };
    match root.strip_prefix("\\\\") {
        Some(rest) => {
            let rest = rest.trim_end_matches('\\');
            let (host, share) = rest.split_once('\\').unwrap_or((rest, ""));
            format!("{}~{}", safe(host), safe(share))
        }
        None => format!("local~{}", safe(root.trim_end_matches(['\\', ':']))),
    }
}

/// The share (`\\host\share`) or drive root (`C:\`) a path is on.
pub fn root_of(path: &str) -> String {
    match path.strip_prefix("\\\\") {
        Some(rest) => {
            let mut it = rest.splitn(3, '\\');
            match (it.next(), it.next()) {
                (Some(h), Some(s)) if !s.is_empty() => format!("\\\\{h}\\{s}"),
                (Some(h), _) => format!("\\\\{h}"),
                _ => path.to_string(),
            }
        }
        None => format!("{}\\", path.chars().take(2).collect::<String>()),
    }
}

/// IPv4 address of a host, or the host itself when it already is one.
/// Gives up after two seconds: a dead name server must not hold the walk.
pub fn host_address(host: &str) -> Option<String> {
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Some(host.to_string());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let h = host.to_string();
    std::thread::spawn(move || {
        use std::net::ToSocketAddrs;
        let ip = (h.as_str(), 445).to_socket_addrs().ok().and_then(|mut a| a.find(|x| x.is_ipv4())).map(|a| a.ip().to_string());
        let _ = tx.send(ip);
    });
    rx.recv_timeout(std::time::Duration::from_secs(2)).ok().flatten()
}

#[cfg(windows)]
mod win {
    use super::{Drive, DriveKind};
    use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    use windows::core::PCWSTR;

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn list_drives() -> Vec<Drive> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = format!("{}:", (b'A' + i as u8) as char);
            let root = wide(&format!("{letter}\\"));
            let kind = match unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } {
                DRIVE_FIXED => DriveKind::Fixed,
                DRIVE_REMOVABLE => DriveKind::Removable,
                DRIVE_REMOTE => DriveKind::Network,
                _ => DriveKind::Other,
            };
            let unc = if kind == DriveKind::Network { unc_of(&letter) } else { None };
            out.push(Drive { letter, kind, unc });
        }
        out
    }

    fn unc_of(letter: &str) -> Option<String> {
        let local = wide(letter);
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let rc = unsafe {
            WNetGetConnectionW(PCWSTR(local.as_ptr()), Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len)
        };
        if rc.0 != 0 {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_share_by_name_and_by_address_is_one_root() {
        let net = |l: &str, u: &str| Drive { letter: l.into(), kind: DriveKind::Network, unc: Some(u.into()) };
        let mut m = DriveMap {
            drives: vec![
                net("P:", r"\\Docsrv-1\DOCS-LATEST"),
                net("T:", r"\\10.0.0.54\docs-latest"),
                net("Y:", r"\\Docsrv-1\p20002"),
                net("Z:", r"\\10.0.0.20\Project"),
                net("Q:", r"\\gone-host\x"),
            ],
        };
        let address = |h: &str| match h.to_lowercase().as_str() {
            "docsrv-1" => Some("10.0.0.54".to_string()),
            "gone-host" => None,
            ip => Some(ip.to_string()),
        };
        let merged = m.merge_aliases(address);
        assert_eq!(merged, vec![(r"\\10.0.0.54\docs-latest".to_string(), r"\\Docsrv-1\DOCS-LATEST".to_string())]);
        assert_eq!(m.to_unc(r"T:\00_DOCS\a.pdf"), r"\\Docsrv-1\DOCS-LATEST\00_DOCS\a.pdf");
        // Another share on the same server stays apart.
        assert_eq!(m.to_unc(r"Y:\a"), r"\\Docsrv-1\p20002\a");
        assert_eq!(m.to_unc(r"Q:\a"), r"\\gone-host\x\a");
        assert_eq!(host_address("10.0.0.9").as_deref(), Some("10.0.0.9"));
    }

    #[test]
    fn every_pc_writes_a_share_the_same_way() {
        let address = |h: &str| match h.to_lowercase().as_str() {
            "docsrv-1" => Some("10.0.0.54".to_string()),
            "gone-host" => None,
            ip => Some(ip.to_string()),
        };
        assert_eq!(canonical_path(r"\\Docsrv-1\Docs\J1\a.pdf", &address), r"\\10.0.0.54\Docs\J1\a.pdf");
        assert_eq!(canonical_path(r"\\Docsrv-1", &address), r"\\10.0.0.54");
        assert_eq!(canonical_path(r"\\gone-host\x\a", &address), r"\\gone-host\x\a");
        assert_eq!(canonical_path(r"C:\Work\a.pdf", &address), r"C:\Work\a.pdf");
        assert_eq!(root_key(r"\\10.0.0.54\Docs Latest\"), "10.0.0.54~docs_latest");
        assert_eq!(root_key(r"C:\"), "local~c");
        assert_eq!(root_of(r"\\10.0.0.54\Docs\J1\a.pdf"), r"\\10.0.0.54\Docs");
        assert_eq!(root_of(r"d:\scans\a.pdf"), r"d:\");
        let mut m = DriveMap { drives: vec![Drive { letter: "P:".into(), kind: DriveKind::Network, unc: Some(r"\\Docsrv-1\Docs".into()) }] };
        m.canonicalize(&address);
        assert_eq!(m.to_unc(r"P:\a.pdf"), r"\\10.0.0.54\Docs\a.pdf");
        assert_eq!(m.to_display(r"\\10.0.0.54\docs\a.pdf"), r"P:\a.pdf");
    }

    fn map() -> DriveMap {
        DriveMap {
            drives: vec![
                Drive { letter: "W:".into(), kind: DriveKind::Network, unc: Some(r"\\fs01\projects".into()) },
                Drive { letter: "U:".into(), kind: DriveKind::Network, unc: Some(r"\\fs02\exchange".into()) },
                Drive { letter: "X:".into(), kind: DriveKind::Network, unc: Some(r"\\fs02\e".into()) },
                Drive { letter: "C:".into(), kind: DriveKind::Fixed, unc: None },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let m = map();
        let unc = m.to_unc(r"w:\Isometrics\a.pdf");
        assert_eq!(unc, r"\\fs01\projects\Isometrics\a.pdf");
        assert_eq!(m.to_display(&unc), r"W:\Isometrics\a.pdf");
        assert_eq!(m.to_unc(r"C:\x.pdf"), r"C:\x.pdf");
    }

    #[test]
    fn unmapped_share_stays_unc() {
        assert_eq!(map().to_display(r"\\docsrv-1\p20002\a.pdf"), r"\\docsrv-1\p20002\a.pdf");
    }

    #[test]
    fn share_prefix_needs_boundary() {
        // \\fs02\e must not claim \\fs02\exports
        assert_eq!(map().to_display(r"\\fs02\exports\a.pdf"), r"\\fs02\exports\a.pdf");
    }

    #[test]
    fn same_server_same_key() {
        assert_eq!(server_key(r"\\fs02\e\x"), server_key(r"\\fs02\exchange"));
        assert_eq!(server_key(r"d:\scans"), "local:D:");
    }
}
