//! In-app updates from the public GitHub releases of this repository.
//!
//! Each release carries `latest.json` (version, notes, installer URL and the
//! installer's SHA-256). The installer is downloaded over HTTPS, checked
//! against that SHA-256, and only then run. No signing keys or secrets are
//! involved. Settings and the index live in %APPDATA% / %LOCALAPPDATA% and
//! are never touched by the installer.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::PathBuf;

const REPO: &str = "https://github.com/dexterity-admin/Sleuth/releases";

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    pub url: String,
    pub sha256: String,
    /// The release before this one, for rolling back.
    #[serde(default)]
    pub previous: Option<Previous>,
}

/// The release before the current one, as listed in `latest.json`.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Previous {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

impl From<Previous> for Manifest {
    fn from(p: Previous) -> Manifest {
        Manifest { version: p.version, notes: String::new(), url: p.url, sha256: p.sha256, previous: None }
    }
}

fn manifest_url(channel: &str) -> String {
    if channel == "dev" { format!("{REPO}/download/dev/latest.json") } else { format!("{REPO}/latest/download/latest.json") }
}

fn fetch(channel: &str) -> Result<Manifest, String> {
    let body = ureq::get(&manifest_url(channel))
        .call()
        .map_err(|e| format!("could not check for updates: {e}"))?
        .into_body()
        .read_to_string()
        .map_err(|e| format!("could not read the update information: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("the update information is not valid: {e}"))
}

fn version(v: &str) -> Result<semver::Version, String> {
    semver::Version::parse(v.trim_start_matches('v')).map_err(|e| format!("version {v}: {e}"))
}

/// The newer release on `channel`, if there is one.
pub fn check(channel: &str, current: &str) -> Result<Option<Manifest>, String> {
    let m = fetch(channel)?;
    let newer = version(&m.version)? > version(current)?;
    Ok(newer.then_some(m))
}

/// The release before the current one on `channel`, only when it is older
/// than the running version. Downloaded and checked exactly like an update.
pub fn rollback_target(channel: &str, current: &str) -> Result<Option<Manifest>, String> {
    let Some(prev) = fetch(channel)?.previous else { return Ok(None) };
    let older = version(&prev.version)? < version(current)?;
    Ok(older.then(|| prev.into()))
}

/// Download the installer and verify it. Returns the verified file.
pub fn download(m: &Manifest, progress: &dyn Fn(u64, Option<u64>)) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("Sleuth-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("Sleuth_{}_setup.exe", m.version));
    let part = file.with_extension("part");
    // Installers of other versions are no longer needed.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.path() != file && e.path() != part {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    // Downloaded earlier (in the background, before the user was asked).
    if file.is_file() && sha256_of(&file).is_some_and(|h| h.eq_ignore_ascii_case(m.sha256.trim())) {
        let n = std::fs::metadata(&file).map(|md| md.len()).unwrap_or(0);
        progress(n, Some(n));
        return Ok(file);
    }
    let resp = ureq::get(&m.url).call().map_err(|e| format!("download failed: {e}"))?;
    let total = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    let mut reader = resp.into_body().into_reader();
    let mut out = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        progress(done, total);
    }
    drop(out);
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if !got.eq_ignore_ascii_case(m.sha256.trim()) {
        let _ = std::fs::remove_file(&part);
        return Err("the downloaded update failed its checksum and was discarded".into());
    }
    std::fs::rename(&part, &file).map_err(|e| e.to_string())?;
    Ok(file)
}

fn sha256_of(file: &PathBuf) -> Option<String> {
    let mut f = std::fs::File::open(file).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Run the installer in update mode: it shows only a progress bar, replaces
/// the program files and starts Sleuth again.
pub fn run_installer(file: &PathBuf) -> Result<(), String> {
    std::process::Command::new(file).args(["/P", "/UPDATE", "/R"]).spawn().map(|_| ()).map_err(|e| format!("could not start the installer: {e}"))
}
