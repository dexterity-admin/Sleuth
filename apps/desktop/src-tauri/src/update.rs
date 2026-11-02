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
}

fn manifest_url(channel: &str) -> String {
    if channel == "dev" { format!("{REPO}/download/dev/latest.json") } else { format!("{REPO}/latest/download/latest.json") }
}

/// The newer release on `channel`, if there is one.
pub fn check(channel: &str, current: &str) -> Result<Option<Manifest>, String> {
    let body = ureq::get(&manifest_url(channel))
        .call()
        .map_err(|e| format!("could not check for updates: {e}"))?
        .into_body()
        .read_to_string()
        .map_err(|e| format!("could not read the update information: {e}"))?;
    let m: Manifest = serde_json::from_str(&body).map_err(|e| format!("the update information is not valid: {e}"))?;
    let newer = semver::Version::parse(&m.version).map_err(|e| e.to_string())? > semver::Version::parse(current).map_err(|e| e.to_string())?;
    Ok(newer.then_some(m))
}

/// Download the installer and verify it. Returns the verified file.
pub fn download(m: &Manifest, progress: &dyn Fn(u64, Option<u64>)) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("Sleuth-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("Sleuth_{}_setup.exe", m.version));
    let part = file.with_extension("part");
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

/// Run the installer in update mode: it shows only a progress bar, replaces
/// the program files and starts Sleuth again.
pub fn run_installer(file: &PathBuf) -> Result<(), String> {
    std::process::Command::new(file).args(["/P", "/UPDATE", "/R"]).spawn().map(|_| ()).map_err(|e| format!("could not start the installer: {e}"))
}
