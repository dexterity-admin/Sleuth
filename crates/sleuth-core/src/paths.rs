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
        assert_eq!(map().to_display(r"\\pdsserver-1\p14414\a.pdf"), r"\\pdsserver-1\p14414\a.pdf");
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
