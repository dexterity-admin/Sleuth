//! Locating and downloading the OCR model files.
//!
//! Standard (PP-OCRv6 small) ships inside the installer, so OCR works offline
//! from the first run. High (PP-OCRv6 medium) is downloaded on first use into
//! `%LOCALAPPDATA%\Sleuth\models` and checked against pinned SHA-256 sums.

use anyhow::{Result, anyhow, bail};
use sleuth_core::settings::{OcrQuality, data_dir};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ModelSet {
    pub name: String,
    pub det: PathBuf,
    pub rec: PathBuf,
    pub dict: PathBuf,
    pub textline_ori: Option<PathBuf>,
}

struct Pinned {
    file: &'static str,
    url: &'static str,
    sha256: &'static str,
}

const BASE: &str = "https://github.com/GreatV/oar-ocr/releases/download";

const HIGH: [Pinned; 2] = [
    Pinned {
        file: "pp-ocrv6_medium_det.onnx",
        url: "v0.7.0/pp-ocrv6_medium_det.onnx",
        sha256: "eb13b44b25bb36f89528b68720af8a61d9cf381176107f465db1757b65d086e1",
    },
    Pinned {
        file: "pp-ocrv6_medium_rec.onnx",
        url: "v0.7.0/pp-ocrv6_medium_rec.onnx",
        sha256: "9c09abf0957f7968c7586464b7397b84ad2387a0497a351af40e9acc71b673ba",
    },
];

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

/// Folders that may hold model files, most specific first.
fn search_dirs() -> Vec<PathBuf> {
    let mut v = vec![models_dir()];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        v.push(dir.join("resources").join("models"));
        v.push(dir.join("models"));
    }
    v.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/models"));
    v
}

fn find(file: &str) -> Option<PathBuf> {
    search_dirs().into_iter().map(|d| d.join(file)).find(|p| p.is_file())
}

fn set(size: &str) -> Option<ModelSet> {
    Some(ModelSet {
        name: format!("PP-OCRv6 {size}"),
        det: find(&format!("pp-ocrv6_{size}_det.onnx"))?,
        rec: find(&format!("pp-ocrv6_{size}_rec.onnx"))?,
        dict: find("ppocrv6_dict.txt")?,
        textline_ori: find("pp-lcnet_x1_0_textline_ori.onnx"),
    })
}

#[cfg(windows)]
pub fn total_ram_gb() -> f64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { m.ullTotalPhys as f64 / 1_073_741_824.0 } else { 0.0 }
}

#[cfg(not(windows))]
pub fn total_ram_gb() -> f64 {
    8.0
}

/// The models to use now, and whether the high-accuracy set should be
/// downloaded in the background for next time.
pub fn choose(quality: OcrQuality) -> Result<(ModelSet, bool)> {
    let want_high = match quality {
        OcrQuality::High => true,
        OcrQuality::Standard => false,
        // Measured on piping isometrics: the medium models were 3.4x slower and
        // found no more text than the small ones, so automatic means standard.
        OcrQuality::Auto => false,
    };
    if want_high {
        if let Some(s) = set("medium") {
            return Ok((s, false));
        }
    }
    let s = set("small").ok_or_else(|| anyhow!("OCR model files are missing; reinstall Sleuth"))?;
    Ok((s, want_high))
}

/// Download the high-accuracy models. Files are written to a `.part` file,
/// verified, then renamed, so a broken download never leaves a bad model.
pub fn download_high() -> Result<()> {
    let dir = models_dir();
    std::fs::create_dir_all(&dir)?;
    for p in HIGH.iter() {
        let dest = dir.join(p.file);
        if dest.is_file() && sha256_file(&dest)? == p.sha256 {
            continue;
        }
        let part = dir.join(format!("{}.part", p.file));
        let url = format!("{BASE}/{}", p.url);
        let resp = ureq::get(&url).call().map_err(|e| anyhow!("download {url}: {e}"))?;
        let mut reader = resp.into_body().into_reader();
        let mut out = std::fs::File::create(&part)?;
        std::io::copy(&mut reader, &mut out)?;
        drop(out);
        let got = sha256_file(&part)?;
        if got != p.sha256 {
            let _ = std::fs::remove_file(&part);
            bail!("{} failed its checksum", p.file);
        }
        std::fs::rename(&part, &dest)?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
