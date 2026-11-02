//! Choosing where OCR runs: the CPU, or a graphics card through DirectML.
//!
//! The CPU always works and is the baseline. A graphics card is used only
//! after it has passed a check on this PC: the same fixed picture is read on
//! both, the readings must be identical (text and position), and the card must
//! be clearly faster. The outcome is remembered per adapter, driver, model and
//! Sleuth version, so the check runs once, not at every start. A card that
//! fails later while reading is recorded as failed and the CPU takes over.
//!
//! CUDA and OpenVINO are not offered: they need runtimes that are not part of
//! Windows and that Sleuth does not ship. DirectML is part of Windows 10/11.

use crate::models::ModelSet;
use crate::ocr::{Backend, OcrOptions, OcrReader};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sleuth_core::settings::{OcrBackend, data_dir};
use std::path::PathBuf;
use std::time::Instant;
use tracing::{info, warn};

/// The card must take at most this share of the CPU's time to be chosen.
const MUST_BE_FASTER: f64 = 0.8;
/// Boxes may differ by this many pixels between backends.
const BOX_TOLERANCE: f32 = 3.0;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    /// adapter | driver | model hash | Sleuth version
    pub key: String,
    pub adapter: String,
    pub backend: Option<Backend>,
    pub cpu_ms: u64,
    pub gpu_ms: u64,
    pub note: String,
    pub checked_at: i64,
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub backend: Backend,
    /// For the status bar: "CPU" or "DirectML (adapter)".
    pub label: String,
    /// Why, for the monitor page and the log.
    pub note: String,
}

fn profile_path() -> PathBuf {
    data_dir().join("ocr-backend.json")
}

fn load_profile() -> Option<Profile> {
    let text = std::fs::read_to_string(profile_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_profile(p: &Profile) {
    let path = profile_path();
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec_pretty(p)
        && std::fs::write(&tmp, bytes).is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// First hardware display adapter: (name, driver version), or `None` when
/// there is only the software renderer.
#[cfg(windows)]
pub fn adapter() -> Option<(String, String)> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIDevice, IDXGIFactory1};
    use windows::core::Interface;
    unsafe {
        let f: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0;
        while let Ok(a) = f.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = a.GetDesc1() else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&d.Description).trim_end_matches('\0').trim().to_string();
            if name.contains("Basic Render") {
                continue;
            }
            let driver = a
                .CheckInterfaceSupport(&IDXGIDevice::IID)
                .map(|v| {
                    let v = v as u64;
                    format!("{}.{}.{}.{}", v >> 48, (v >> 32) & 0xffff, (v >> 16) & 0xffff, v & 0xffff)
                })
                .unwrap_or_default();
            return Some((name, driver));
        }
        None
    }
}

#[cfg(not(windows))]
pub fn adapter() -> Option<(String, String)> {
    None
}

fn cpu(note: impl Into<String>) -> Choice {
    Choice { backend: Backend::Cpu, label: "CPU".into(), note: note.into() }
}

/// Decide the backend for `setting`. May run the check (a few seconds) when
/// no remembered profile fits; call it from the reader thread.
pub fn choose(setting: OcrBackend, models: &ModelSet, threads: usize) -> Choice {
    if setting == OcrBackend::Cpu {
        return cpu("CPU only (set in settings)");
    }
    let Some((name, driver)) = adapter() else { return cpu("CPU: no graphics adapter that DirectML can use") };
    let key = format!("{name}|{driver}|{}|{}", models.hash, env!("CARGO_PKG_VERSION"));
    let profile = match load_profile() {
        Some(p) if p.key == key => p,
        _ => {
            let p = check(&name, &key, models, threads);
            save_profile(&p);
            p
        }
    };
    match profile.backend {
        Some(Backend::DirectMl) => Choice { backend: Backend::DirectMl, label: format!("DirectML ({name})"), note: profile.note },
        _ => cpu(profile.note),
    }
}

/// The card failed while reading: never pick it again for this profile.
pub fn record_failure(reason: &str) {
    if let Some(mut p) = load_profile() {
        p.backend = Some(Backend::Cpu);
        p.note = format!("CPU: DirectML failed while reading ({reason})");
        save_profile(&p);
    }
    warn!("DirectML failed while reading, using the CPU from now on: {reason}");
}

fn probe_image() -> Result<image::RgbImage> {
    let img = image::load_from_memory_with_format(include_bytes!("../assets/probe.png"), image::ImageFormat::Png)?;
    Ok(img.to_rgb8())
}

type Reading = Vec<(String, f32, [f32; 4])>;

/// Read the probe three times (the first warms up); time the last two.
fn run(models: &ModelSet, opts: &OcrOptions, img: &image::RgbImage) -> Result<(Reading, u64)> {
    let r = OcrReader::build(models, opts)?;
    let first = r.recognize(img.clone())?;
    let t = Instant::now();
    for _ in 0..2 {
        r.recognize(img.clone())?;
    }
    Ok((first, t.elapsed().as_millis() as u64 / 2))
}

/// Same text in the same places (confidence may differ in the last digits).
pub fn same_reading(a: &Reading, b: &Reading) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // Pair every region of `a` with an unused region of `b` that has the
    // same text and nearly the same box.
    let mut used = vec![false; b.len()];
    a.iter().all(|(t, _, ba)| {
        let hit = b.iter().enumerate().position(|(j, (u, _, bb))| !used[j] && t == u && ba.iter().zip(bb).all(|(p, q)| (p - q).abs() <= BOX_TOLERANCE));
        hit.map(|j| used[j] = true).is_some()
    })
}

fn check(adapter: &str, key: &str, models: &ModelSet, threads: usize) -> Profile {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let mut p = Profile { key: key.into(), adapter: adapter.into(), checked_at: now, ..Default::default() };
    let img = match probe_image() {
        Ok(i) => i,
        Err(e) => {
            p.note = format!("CPU: check picture unreadable ({e})");
            return p;
        }
    };
    let base = OcrOptions { threads, ..Default::default() };
    let (cpu_read, cpu_ms) = match run(models, &base, &img) {
        Ok(x) => x,
        Err(e) => {
            p.note = format!("CPU: check could not run ({e:#})");
            return p;
        }
    };
    p.cpu_ms = cpu_ms;
    let gpu = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(models, &OcrOptions { backend: Backend::DirectMl, ..base.clone() }, &img)));
    match gpu {
        Err(_) => p.note = format!("CPU: DirectML crashed during the check on {adapter}"),
        Ok(Err(e)) => p.note = format!("CPU: DirectML is not usable on {adapter} ({e:#})"),
        Ok(Ok((gpu_read, gpu_ms))) => {
            p.gpu_ms = gpu_ms;
            if !same_reading(&cpu_read, &gpu_read) {
                p.note = format!("CPU: DirectML on {adapter} read the check picture differently from the CPU");
            } else if (gpu_ms as f64) > cpu_ms as f64 * MUST_BE_FASTER {
                p.note = format!("CPU: DirectML on {adapter} gave the same reading but was not faster ({gpu_ms} ms against {cpu_ms} ms on the CPU)");
            } else {
                p.backend = Some(Backend::DirectMl);
                p.note = format!("DirectML on {adapter}: same reading as the CPU, {gpu_ms} ms against {cpu_ms} ms");
            }
        }
    }
    if p.backend.is_none() {
        p.backend = Some(Backend::Cpu);
    }
    info!("OCR backend check: {}", p.note);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_compare_text_and_place() {
        let a: Reading = vec![("PSV-1203".into(), 0.99, [10.0, 10.0, 90.0, 30.0]), ("REV. A".into(), 0.9, [10.0, 50.0, 60.0, 70.0])];
        let mut b = a.clone();
        b.reverse();
        b[0].1 = 0.91;
        b[1].2[0] = 12.0;
        assert!(same_reading(&a, &b));
        b[1].0 = "PSV-12O3".into();
        assert!(!same_reading(&a, &b));
        let mut c = a.clone();
        c[0].2[2] = 120.0;
        assert!(!same_reading(&a, &c));
    }
}
