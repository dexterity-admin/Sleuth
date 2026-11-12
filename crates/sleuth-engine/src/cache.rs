//! Content hashing and the optional office-wide shared cache of read results.
//!
//! The shared cache is a plain folder on a network drive. A read PDF is stored
//! under a key made from its content hash *and* the pipeline fingerprint
//! (reader version, OCR model hash, OCR settings, resolution), so a changed
//! model or setting never reuses old readings:
//! `<dir>\v2\<first two hex>\<key>.dhx` (gzipped JSON with a checksum).
//!
//! - Files are written under a temporary name and renamed, so a reader on
//!   another PC never sees half a file.
//! - A damaged, older-format or mismatched entry is ignored (and re-read).
//! - Before reading a PDF itself, a PC takes a *claim*: `<key>.claim`,
//!   created exclusively, with an expiry it renews while reading. Another PC
//!   seeing a live claim waits for the result instead of reading the same file
//!   at the same time. An expired claim (crashed or switched-off PC) is taken
//!   over.

use crate::text::FileText;
use anyhow::Result;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sleuth_core::paths::DriveMap;
use sleuth_core::protocol::CacheCheck;
use std::io::{Read, Write};
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

const FORMAT: u32 = 2;
/// A claim is renewed after every page; this long without renewal and it
/// is considered abandoned.
pub const CLAIM_LEASE: Duration = Duration::from_secs(20 * 60);

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn me() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this PC".into())
}

#[derive(Serialize, Deserialize)]
struct Record {
    format: u32,
    content_hash: String,
    pipeline: String,
    page_count: u32,
    created: i64,
    created_by: String,
    status: String,
    /// BLAKE3 of the JSON of `text`.
    checksum: String,
    text: FileText,
}

#[derive(Debug, Serialize, Deserialize)]
struct ClaimFile {
    host: String,
    #[serde(default)]
    user: String,
    pid: u32,
    until: i64,
}

fn user() -> String {
    std::env::var("USERNAME").unwrap_or_default()
}

/// A claim this PC holds; released when dropped.
pub struct Claim {
    path: PathBuf,
}

impl Claim {
    /// Push the expiry out again (call between pages).
    pub fn renew(&self) {
        let c = ClaimFile { host: me(), user: user(), pid: std::process::id(), until: now() + CLAIM_LEASE.as_secs() as i64 };
        if let Ok(b) = serde_json::to_vec(&c) {
            let _ = std::fs::write(&self.path, b);
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub enum ClaimResult {
    Mine(Claim),
    /// Another PC is reading this file; its claim runs until `until`.
    HeldBy { host: String, until: i64 },
    /// The folder could not be written; read without a claim.
    Unavailable,
}

pub struct SharedCache {
    dir: PathBuf,
}

/// The cache key: content and pipeline together.
pub fn cache_key(content_hash: &str, pipeline: &str) -> String {
    blake3::hash(format!("{content_hash}:{pipeline}").as_bytes()).to_hex().to_string()
}

impl SharedCache {
    pub fn new(dir: impl Into<PathBuf>) -> SharedCache {
        SharedCache { dir: dir.into() }
    }

    fn entry(&self, key: &str, ext: &str) -> PathBuf {
        self.dir.join("v2").join(&key[..2.min(key.len())]).join(format!("{key}.{ext}"))
    }

    pub fn get(&self, content_hash: &str, pipeline: &str) -> Option<FileText> {
        let f = std::fs::File::open(self.entry(&cache_key(content_hash, pipeline), "dhx")).ok()?;
        let mut s = String::new();
        GzDecoder::new(f).read_to_string(&mut s).ok()?;
        let r: Record = serde_json::from_str(&s).ok()?;
        if r.format != FORMAT || r.content_hash != content_hash || r.pipeline != pipeline || r.status != "complete" {
            return None;
        }
        let sum = blake3::hash(&serde_json::to_vec(&r.text).ok()?).to_hex().to_string();
        (sum == r.checksum).then_some(r.text)
    }

    pub fn put(&self, content_hash: &str, pipeline: &str, text: &FileText) -> Result<()> {
        let dest = self.entry(&cache_key(content_hash, pipeline), "dhx");
        if dest.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(dest.parent().unwrap())?;
        let json = serde_json::to_vec(text)?;
        let rec = Record {
            format: FORMAT,
            content_hash: content_hash.into(),
            pipeline: pipeline.into(),
            page_count: text.pages.len() as u32,
            created: now(),
            created_by: me(),
            status: "complete".into(),
            checksum: blake3::hash(&json).to_hex().to_string(),
            text: text.clone(),
        };
        let tmp = dest.with_extension(format!("{}.{}.tmp", me(), std::process::id()));
        {
            let f = std::fs::File::create(&tmp)?;
            let mut gz = GzEncoder::new(f, Compression::default());
            gz.write_all(&serde_json::to_vec(&rec)?)?;
            gz.finish()?.sync_all()?;
        }
        if std::fs::rename(&tmp, &dest).is_err() {
            // Another PC finished the same file first.
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(())
    }

    /// Try to become the PC that reads this file.
    pub fn claim(&self, content_hash: &str, pipeline: &str) -> ClaimResult {
        let path = self.entry(&cache_key(content_hash, pipeline), "claim");
        if std::fs::create_dir_all(path.parent().unwrap()).is_err() {
            return ClaimResult::Unavailable;
        }
        for _ in 0..2 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let c = ClaimFile { host: me(), user: user(), pid: std::process::id(), until: now() + CLAIM_LEASE.as_secs() as i64 };
                    let _ = f.write_all(&serde_json::to_vec(&c).unwrap_or_default());
                    return ClaimResult::Mine(Claim { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let held: Option<ClaimFile> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
                    match held {
                        // Live, and not a leftover of this user's earlier (crashed)
                        // worker on this PC: one worker runs per user.
                        Some(c) if c.until > now() && !(c.host == me() && c.user == user() && c.pid != std::process::id()) => {
                            return ClaimResult::HeldBy { host: c.host, until: c.until };
                        }
                        Some(_) => {
                            let _ = std::fs::remove_file(&path);
                        }
                        // Unreadable: being written this instant, or garbage. Judge by age.
                        None => {
                            let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).unwrap_or_default();
                            if age < CLAIM_LEASE {
                                return ClaimResult::HeldBy { host: "another PC".into(), until: now() + 120 };
                            }
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                Err(_) => return ClaimResult::Unavailable,
            }
        }
        ClaimResult::HeldBy { host: "another PC".into(), until: now() + 120 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::READER_VERSION;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sl-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trip_and_pipeline_check() {
        let d = dir("rt");
        let c = SharedCache::new(&d);
        let ft = FileText { reader_version: READER_VERSION, model: "m".into(), ..Default::default() };
        c.put("abcdef", "pipe1", &ft).unwrap();
        assert_eq!(c.get("abcdef", "pipe1").unwrap().model, "m");
        assert!(c.get("abcdef", "pipe2").is_none(), "another pipeline never reuses it");
        assert!(c.get("ffffff", "pipe1").is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn damaged_entry_is_ignored() {
        let d = dir("bad");
        let c = SharedCache::new(&d);
        let p = c.entry(&cache_key("abc", "p"), "dhx");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"not gzip").unwrap();
        assert!(c.get("abc", "p").is_none());
        // a checksum that does not fit the content
        let rec = Record { format: FORMAT, content_hash: "abc".into(), pipeline: "p".into(), page_count: 0, created: 0, created_by: "x".into(), status: "complete".into(), checksum: "0".repeat(64), text: FileText::default() };
        let mut gz = GzEncoder::new(std::fs::File::create(&p).unwrap(), Compression::default());
        gz.write_all(&serde_json::to_vec(&rec).unwrap()).unwrap();
        gz.finish().unwrap();
        assert!(c.get("abc", "p").is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn claims_exclude_and_expire() {
        let d = dir("claim");
        let c = SharedCache::new(&d);
        let ClaimResult::Mine(first) = c.claim("h", "p") else { panic!("first claim") };
        assert!(matches!(c.claim("h", "p"), ClaimResult::HeldBy { .. }), "held while the first lives");
        drop(first);
        assert!(matches!(c.claim("h", "p"), ClaimResult::Mine(_)), "released on drop");
        let path = c.entry(&cache_key("h2", "p"), "claim");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&ClaimFile { host: "OTHER-PC".into(), user: "u".into(), pid: 1, until: now() - 5 }).unwrap()).unwrap();
        assert!(matches!(c.claim("h2", "p"), ClaimResult::Mine(_)), "expired claim is taken over");
        std::fs::write(&path, serde_json::to_vec(&ClaimFile { host: "OTHER-PC".into(), user: "u".into(), pid: 1, until: now() + 600 }).unwrap()).unwrap();
        match c.claim("h2", "p") {
            ClaimResult::HeldBy { host, .. } => assert_eq!(host, "OTHER-PC"),
            _ => panic!("live claim must be respected"),
        }
        let _ = std::fs::remove_dir_all(d);
    }
}

// Check that the shared cache folder can be used from this PC: the drive
/// letter is mapped, the server answers, and the folder can be created and
/// written. Each probe that can hang on a network share runs with a timeout.
pub fn check_cache(dir: Option<&str>, enabled: bool, drives: &DriveMap) -> CacheCheck {
    use sleuth_core::protocol::CacheState as S;
    let check = |state, path: &str, message: &str| CacheCheck { state, path: path.to_string(), message: message.to_string(), office_server: None };
    if !enabled {
        return check(S::Disabled, "", "switched off");
    }
    let Some(dir) = dir.map(str::trim).filter(|d| !d.is_empty()) else {
        return check(S::NotSet, "", "no shared folder is chosen");
    };
    let unc = drives.to_unc(dir);
    // A folder on this PC's own disk is no use to an office, but it is what
    // the office server on its own disks, a one-PC setup and the tests want.
    if !unc.starts_with("\\\\") && std::env::var_os("SLEUTH_LOCAL_STORE").is_some() {
        return match std::fs::create_dir_all(&unc) {
            Ok(()) => check(S::Ok, &unc, "working (a folder on this PC)"),
            Err(e) => check(S::NoAccess, &unc, &format!("cannot create it: {e}")),
        };
    }
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

/// `check_cache`, then, when the folder cannot be reached as written (a drive
/// letter that is not mapped on this PC, or a share that does not answer), the
/// same folder by its network path (`fallback`). A working folder also brings
/// the office server's connection card, when the server's setup left one.
pub fn check_cache_with(dir: Option<&str>, fallback: Option<&str>, enabled: bool, drives: &DriveMap) -> CacheCheck {
    use sleuth_core::protocol::CacheState as S;
    let mut c = check_cache(dir, enabled, drives);
    if let (S::NotSet | S::Offline, Some(dir), Some(alt)) = (c.state, dir.map(str::trim).filter(|d| !d.is_empty()), fallback.map(str::trim)) {
        if !alt.is_empty() && !alt.eq_ignore_ascii_case(&c.path) && !alt.eq_ignore_ascii_case(dir) {
            let a = check_cache(Some(alt), true, drives);
            if a.state == S::Ok {
                let why = c.message.clone();
                c = CacheCheck { message: format!("working through the network path, because {why}"), ..a };
            }
        }
    }
    if c.state == S::Ok {
        c.office_server = read_server_card(Path::new(&c.path));
    }
    c
}

/// The connection card `sleuth-server setup` writes into the index folder.
pub fn read_server_card(dir: &Path) -> Option<sleuth_core::protocol::OfficeServerCard> {
    let p = dir.join(crate::remote::SERVER_CARD_FILE);
    let text = with_timeout(move || std::fs::read_to_string(p), Duration::from_secs(5))?.ok()?;
    serde_json::from_str::<sleuth_core::protocol::OfficeServerCard>(text.trim_start_matches('\u{feff}')).ok().filter(|c| c.address.starts_with("https://"))
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

    #[test]
    fn a_fallback_that_fails_too_keeps_the_first_answer() {
        let drives = DriveMap { drives: vec![Drive { letter: "C:".into(), kind: DriveKind::Fixed, unc: None }] };
        let c = check_cache_with(Some(r"U:\Sleuth-cache"), Some(r"\\no-such-host-sleuth-test\share\Sleuth-cache"), true, &drives);
        assert_eq!(c.state, CacheState::NotSet);
        assert!(c.message.contains("not mapped"), "{}", c.message);
        assert!(c.office_server.is_none());
    }

    /// Run by hand on an office PC: `cargo test -p sleuth-engine office_unc -- --ignored`.
    #[test]
    #[ignore]
    fn office_unc_is_used_when_u_is_not_mapped() {
        let drives = DriveMap { drives: vec![Drive { letter: "C:".into(), kind: DriveKind::Fixed, unc: None }] };
        let c = check_cache_with(Some(sleuth_core::settings::OFFICE_CACHE_DIR), Some(sleuth_core::settings::OFFICE_CACHE_UNC), true, &drives);
        assert_eq!(c.state, CacheState::Ok, "{}", c.message);
        assert!(c.path.starts_with(r"\\"), "{}", c.path);
        assert!(c.message.starts_with("working through"), "{}", c.message);
    }

    #[test]
    fn server_card_is_read_from_the_folder() {
        let d = std::env::temp_dir().join(format!("sleuth-card-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(read_server_card(&d).is_none());
        std::fs::write(d.join(crate::remote::SERVER_CARD_FILE), r#"{"address":"https://srv:8455","pc":"SRV","at":5}"#).unwrap();
        let c = read_server_card(&d).unwrap();
        assert_eq!((c.address.as_str(), c.pc.as_str(), c.at), ("https://srv:8455", "SRV", 5));
        std::fs::write(d.join(crate::remote::SERVER_CARD_FILE), r#"{"address":"ftp://x"}"#).unwrap();
        assert!(read_server_card(&d).is_none(), "only https addresses are offered");
        let _ = std::fs::remove_dir_all(&d);
    }
}
