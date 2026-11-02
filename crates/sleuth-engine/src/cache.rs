//! Content hashing and the optional office-wide shared cache of read results.
//!
//! The shared cache is a plain folder on a network drive. Each read PDF is
//! stored as `<dir>\<first two hex>\<blake3>.dhx` (gzipped JSON). Files are
//! written under a temporary name and renamed, so a reader on another PC never
//! sees half a file; damaged or older-format entries are ignored.

use crate::text::{FileText, READER_VERSION};
use anyhow::Result;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use std::io::{Read, Write};
use sleuth_core::paths::DriveMap;
use sleuth_core::protocol::CacheCheck;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

pub struct SharedCache {
    dir: PathBuf,
}

impl SharedCache {
    pub fn new(dir: impl Into<PathBuf>) -> SharedCache {
        SharedCache { dir: dir.into() }
    }

    fn entry(&self, hash: &str) -> PathBuf {
        self.dir.join(&hash[..2.min(hash.len())]).join(format!("{hash}.dhx"))
    }

    pub fn get(&self, hash: &str) -> Option<FileText> {
        let f = std::fs::File::open(self.entry(hash)).ok()?;
        let mut s = String::new();
        GzDecoder::new(f).read_to_string(&mut s).ok()?;
        let ft: FileText = serde_json::from_str(&s).ok()?;
        (ft.reader_version >= READER_VERSION).then_some(ft)
    }

    pub fn put(&self, hash: &str, text: &FileText) -> Result<()> {
        let dest = self.entry(hash);
        if dest.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(dest.parent().unwrap())?;
        let user = std::env::var("COMPUTERNAME").unwrap_or_default();
        let tmp = dest.with_extension(format!("{user}.{}.tmp", std::process::id()));
        {
            let f = std::fs::File::create(&tmp)?;
            let mut gz = GzEncoder::new(f, Compression::default());
            gz.write_all(&serde_json::to_vec(text)?)?;
            gz.finish()?.sync_all()?;
        }
        if std::fs::rename(&tmp, &dest).is_err() {
            // Another PC finished the same file first.
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_version_check() {
        let dir = std::env::temp_dir().join(format!("dh-cache-{}", std::process::id()));
        let c = SharedCache::new(&dir);
        let ft = FileText { reader_version: READER_VERSION, model: "m".into(), pages: vec![] };
        c.put("abcdef", &ft).unwrap();
        assert_eq!(c.get("abcdef").unwrap().model, "m");
        assert!(c.get("ffffff").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Check that the shared cache folder can be used from this PC: the drive
/// letter is mapped, the server answers, and the folder can be created and
/// written. Each probe that can hang on a network share runs with a timeout.
pub fn check_cache(dir: Option<&str>, enabled: bool, drives: &DriveMap) -> CacheCheck {
    use sleuth_core::protocol::CacheState as S;
    let check = |state, path: &str, message: &str| CacheCheck { state, path: path.to_string(), message: message.to_string() };
    if !enabled {
        return check(S::Disabled, "", "switched off");
    }
    let Some(dir) = dir.map(str::trim).filter(|d| !d.is_empty()) else {
        return check(S::NotSet, "", "no shared folder is chosen");
    };
    let unc = drives.to_unc(dir);
    if !unc.starts_with("\\\\") {
        let letter: String = dir.chars().take(2).collect();
        if dir.chars().nth(1) == Some(':') && !drives.drives.iter().any(|d| d.letter.eq_ignore_ascii_case(&letter)) {
            return check(S::NotSet, dir, &format!("drive {letter} is not mapped on this PC"));
        }
        if drives.drives.iter().any(|d| d.letter.eq_ignore_ascii_case(&letter) && d.unc.is_none()) {
            return check(S::NoAccess, dir, &format!("{letter} is a local disk; choose a folder on a network drive so other PCs can use it"));
        }
    }
    // \\server\share must answer first.
    let share: String = {
        let parts: Vec<&str> = unc.trim_start_matches('\\').splitn(3, '\\').collect();
        if parts.len() >= 2 { format!("\\\\{}\\{}", parts[0], parts[1]) } else { unc.clone() }
    };
    let probe = {
        let share = share.clone();
        with_timeout(move || std::fs::read_dir(&share).map(|_| ()), Duration::from_secs(5))
    };
    match probe {
        None => return check(S::Offline, &unc, &format!("{share} does not answer")),
        Some(Err(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return check(S::NoAccess, &unc, &format!("no access to {share}"));
        }
        Some(Err(_)) => return check(S::Offline, &unc, &format!("{share} cannot be reached")),
        Some(Ok(())) => {}
    }
    let target = unc.clone();
    let written = with_timeout(
        move || -> std::io::Result<()> {
            std::fs::create_dir_all(&target)?;
            let pc = std::env::var("COMPUTERNAME").unwrap_or_default();
            let probe = Path::new(&target).join(format!(".write-test-{pc}-{}", std::process::id()));
            std::fs::write(&probe, b"ok")?;
            std::fs::remove_file(&probe)
        },
        Duration::from_secs(10),
    );
    match written {
        Some(Ok(())) => check(S::Ok, &unc, "working"),
        Some(Err(e)) => check(S::NoAccess, &unc, &format!("cannot write there: {e}")),
        None => check(S::Offline, &unc, "the folder did not answer in time"),
    }
}

fn with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static, t: Duration) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(t).ok()
}

#[cfg(test)]
mod check_tests {
    use super::*;
    use sleuth_core::paths::{Drive, DriveKind};
    use sleuth_core::protocol::CacheState;

    #[test]
    fn unmapped_letter_and_local_disk_are_reported() {
        let drives = DriveMap { drives: vec![Drive { letter: "C:".into(), kind: DriveKind::Fixed, unc: None }] };
        assert_eq!(check_cache(Some(r"U:\Sleuth-cache"), true, &drives).state, CacheState::NotSet);
        assert_eq!(check_cache(Some(r"C:\cache"), true, &drives).state, CacheState::NoAccess);
        assert_eq!(check_cache(None, true, &drives).state, CacheState::NotSet);
        assert_eq!(check_cache(None, false, &drives).state, CacheState::Disabled);
    }
}
