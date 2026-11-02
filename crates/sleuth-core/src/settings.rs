//! Per-user settings in `%APPDATA%\Sleuth\settings.json`.
//!
//! Rules that keep settings safe across updates:
//! - every field has a default, so an older file always loads;
//! - unknown keys are kept and written back, so a newer file opened by an
//!   older build loses nothing;
//! - migrations only add or rename, and the previous file is copied to
//!   `settings.v{N}.bak` before a migrated file is written;
//! - writes go to a temporary file that is then renamed over the old one.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;

/// Office default for the shared reading cache.
pub const OFFICE_CACHE_DIR: &str = r"U:\Sleuth-cache";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OcrQuality {
    /// Currently the same as Standard; kept so the choice can change later
    /// without rewriting users' settings.
    #[default]
    Auto,
    /// PP-OCRv6 small models, bundled with the installer.
    Standard,
    /// PP-OCRv6 medium models, downloaded on first use.
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Dev,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    /// Folders whose PDFs are read (text layer + OCR) in the background.
    /// Stored as UNC paths for network locations.
    pub content_scopes: Vec<String>,
    /// Folders never walked or read.
    pub excluded: Vec<String>,
    pub ocr_quality: OcrQuality,
    /// Render resolution for OCR. 300 suits A1/A3 drawings; 400 for tiny print.
    pub ocr_dpi: u32,
    /// Folder on a network drive where finished OCR results are shared
    /// between PCs, so each PDF is read once for the whole office. A drive
    /// letter is resolved to its network path on each PC.
    pub shared_cache_dir: Option<String>,
    /// The shared cache is expected on every PC; the app keeps asking until
    /// it works. Only IT should switch this off (in settings.json).
    pub shared_cache_enabled: bool,
    /// Keep the background reader running after the window is closed.
    pub keep_indexing_when_closed: bool,
    /// Pause OCR while total CPU use is above this percentage.
    pub busy_cpu_percent: u32,
    /// Rebuild the file-name index of each drive this often.
    pub name_rescan_minutes: u32,
    /// Concurrent folder listings allowed per file server during a deep search.
    pub listings_per_server: u32,
    /// Seconds before a single folder listing is given up and reported.
    pub listing_timeout_secs: u32,
    pub update_channel: UpdateChannel,
    /// Keys written by newer versions, preserved untouched.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            schema_version: SCHEMA_VERSION,
            content_scopes: Vec::new(),
            excluded: vec![
                r"C:\Windows".into(),
                r"C:\Program Files".into(),
                r"C:\Program Files (x86)".into(),
                r"C:\ProgramData".into(),
                r"C:\$Recycle.Bin".into(),
            ],
            ocr_quality: OcrQuality::Auto,
            ocr_dpi: 300,
            shared_cache_dir: Some(OFFICE_CACHE_DIR.into()),
            shared_cache_enabled: true,
            keep_indexing_when_closed: true,
            busy_cpu_percent: 60,
            name_rescan_minutes: 360,
            listings_per_server: 4,
            listing_timeout_secs: 20,
            update_channel: UpdateChannel::Stable,
            extra: Map::new(),
        }
    }
}

/// Optional override for both folders, used by tests and portable setups.
fn override_dir(sub: &str) -> Option<PathBuf> {
    std::env::var_os("SLEUTH_DATA_DIR").map(|d| PathBuf::from(d).join(sub))
}

/// `%APPDATA%\Sleuth` (roaming: settings follow the user).
pub fn config_dir() -> PathBuf {
    override_dir("config")
        .unwrap_or_else(|| std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join(crate::APP_NAME))
}

/// `%LOCALAPPDATA%\Sleuth` (machine-local: index, models, logs).
pub fn data_dir() -> PathBuf {
    override_dir("data")
        .unwrap_or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join(crate::APP_NAME))
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

/// Bring an older settings document up to [`SCHEMA_VERSION`]. Each step only
/// adds or renames keys.
fn migrate(mut doc: Map<String, Value>) -> Map<String, Value> {
    let from = doc.get("schema_version").and_then(Value::as_u64).unwrap_or(0) as u32;
    if from < 1 {
        // v0 was never released; nothing to rename.
    }
    doc.insert("schema_version".into(), Value::from(SCHEMA_VERSION.max(from)));
    doc
}

impl Settings {
    pub fn load_from(path: &Path) -> anyhow::Result<Settings> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => return Err(e.into()),
        };
        let doc: Map<String, Value> = match serde_json::from_str(&text) {
            Ok(d) => d,
            Err(_) => {
                // A damaged file is kept for inspection and defaults are used.
                let _ = std::fs::copy(path, path.with_extension("damaged.json"));
                return Ok(Settings::default());
            }
        };
        let old_version = doc.get("schema_version").and_then(Value::as_u64).unwrap_or(0) as u32;
        let migrated = migrate(doc);
        let settings: Settings = serde_json::from_value(Value::Object(migrated))?;
        if old_version < SCHEMA_VERSION {
            let _ = std::fs::copy(path, path.with_file_name(format!("settings.v{old_version}.bak")));
            settings.save_to(path)?;
        }
        Ok(settings)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load() -> anyhow::Result<Settings> {
        Settings::load_from(&settings_path())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&settings_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dh-settings-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("settings.json")
    }

    #[test]
    fn missing_file_gives_defaults() {
        let p = tmp("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(Settings::load_from(&p).unwrap(), Settings::default());
    }

    #[test]
    fn unknown_keys_survive_round_trip() {
        let p = tmp("unknown");
        std::fs::write(&p, r#"{"schema_version":1,"ocr_dpi":400,"from_future":{"a":1}}"#).unwrap();
        let s = Settings::load_from(&p).unwrap();
        assert_eq!(s.ocr_dpi, 400);
        s.save_to(&p).unwrap();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(back["from_future"]["a"], 1);
    }

    #[test]
    fn old_file_is_backed_up_and_upgraded() {
        let p = tmp("old");
        std::fs::write(&p, r#"{"content_scopes":["\\\\srv\\share"]}"#).unwrap();
        let s = Settings::load_from(&p).unwrap();
        assert_eq!(s.content_scopes, vec![r"\\srv\share".to_string()]);
        assert!(p.with_file_name("settings.v0.bak").exists());
        assert_eq!(s.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn damaged_file_falls_back() {
        let p = tmp("damaged");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(Settings::load_from(&p).unwrap(), Settings::default());
    }
}
