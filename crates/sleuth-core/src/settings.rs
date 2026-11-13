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

pub const SCHEMA_VERSION: u32 = 2;

/// Office default for the shared reading cache.
pub const OFFICE_CACHE_DIR: &str = r"U:\Sleuth-cache";

/// The same folder by its network path: used on a PC where U: is not mapped
/// (or does not answer), so nobody has to know the difference.
pub const OFFICE_CACHE_UNC: &str = r"\\192.168.0.200\free exchange - updated\Sleuth-cache";

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

/// Where OCR runs. `Auto` uses a graphics card only after it has passed a
/// correctness and speed check on this PC; anything else runs on the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OcrBackend {
    #[default]
    Auto,
    /// Never use the graphics card (safe mode).
    Cpu,
    /// DirectML on the first graphics adapter, if it passes the check.
    Directml,
}

/// How revisions sort when grouping a drawing's issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RevisionOrder {
    /// A, B, C (pre-issue) before 0, 1, 2 (issued).
    #[default]
    LettersThenNumbers,
    NumbersThenLetters,
}

/// A named set of folders, used to limit searches and group drawings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Project {
    pub name: String,
    /// Folders (UNC for network locations).
    pub roots: Vec<String>,
}

/// One pattern of the engineering tag parser. The first capture group, when
/// present, is the tag; otherwise the whole match is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRule {
    pub category: String,
    pub pattern: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// One title-block field: a value printed after one of `labels`, matching
/// `value` (a regular expression).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldRule {
    pub field: String,
    pub labels: Vec<String>,
    pub value: String,
}

fn yes() -> bool {
    true
}

/// Built-in tag patterns. They are generic piping conventions; offices with
/// their own numbering add or replace rules in settings.
pub fn default_tag_rules() -> Vec<TagRule> {
    let r = |c: &str, p: &str| TagRule { category: c.into(), pattern: p.into(), enabled: true };
    vec![
        r("line", r#"\b\d{1,2}(?:"|''|'|”|″|IN)?-[A-Z]{1,4}-\d{3,6}(?:-[A-Z0-9]{1,6})+\b"#),
        r("drawing", r"\b(?:ISO|PID|P&ID|GA|DWG|DOC|STR|LAY)-[A-Z0-9]+(?:-[A-Z0-9]+)*\b"),
        r("instrument", r"\b(?:[FPTLAHSXZ][A-Z]{1,3})[- ]?\d{3,5}[A-Z]?\b"),
        r("valve", r"\b(?:V|MOV|XV|SDV|BDV)-\d{3,5}[A-Z]?\b"),
        r("equipment", r"\b\d{0,3}[A-Z]{1,2}-\d{3,4}[A-Z]?\b"),
        r("support", r"\b(?:PS|SP|SUP)-?[A-Z]{0,2}-?\d{2,6}\b"),
        r("weld", r"\b(?:FW|SW|BW|W)-\d{1,4}\b"),
        r("spec", r"\b(?:SPEC|PIPING CLASS|CLASS)[ .:]+([A-Z0-9]{2,8})\b"),
    ]
}

/// Built-in title-block fields.
pub fn default_field_rules() -> Vec<FieldRule> {
    let r = |f: &str, labels: &[&str], v: &str| FieldRule { field: f.into(), labels: labels.iter().map(|s| s.to_string()).collect(), value: v.into() };
    vec![
        r("drawing_number", &["DRAWING NUMBER", "DRAWING NO", "DRG NO", "DWG NO", "DOCUMENT NO", "DOC NO"], r"[A-Z0-9][A-Z0-9\-_/.]{3,40}"),
        r("revision", &["REVISION", "REV"], r"[A-Z0-9]{1,3}\b"),
        r("title", &["DRAWING TITLE", "DRG TITLE", "DWG TITLE", "TITLE"], r".{3,100}"),
        r("project", &["PROJECT NUMBER", "PROJECT NO", "JOB NO", "PROJECT"], r".{2,60}"),
        r("sheet", &["SHEET"], r"\d+(?:\s*OF\s*\d+)?"),
        r("discipline", &["DISCIPLINE"], r"[A-Z][A-Z &/-]{2,30}"),
        r("status", &["STATUS"], r"[A-Z][A-Z0-9 -]{2,30}"),
    ]
}

/// Drawing-number prefixes and the discipline they mean, used when the
/// title block does not state one.
pub fn default_discipline_prefixes() -> Vec<(String, String)> {
    [("ISO", "Piping"), ("PID", "Process"), ("P&ID", "Process"), ("GA", "Layout"), ("LAY", "Layout"), ("STR", "Structural"), ("DOC", "Documents")]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// Connection to an optional office Sleuth server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ServerSettings {
    /// `https://host:8455`; empty means standalone.
    pub url: String,
    /// The access key, encrypted for this Windows user (DPAPI, base64).
    pub token_protected: String,
    /// The server's certificate authority (PEM). Empty: look for
    /// `sleuth-server-ca.pem` in the shared cache folder.
    pub ca_file: String,
    /// The server's CA certificate itself (PEM, public), saved when this PC
    /// connected itself from the server's card; used when `ca_file` is empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ca_pem: String,
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
    /// The network path `shared_cache_dir` led to the last time it worked
    /// through a drive letter; tried when that letter is not mapped or does
    /// not answer. Learned by the reader, cleared when the folder changes.
    pub shared_cache_unc: Option<String>,
    /// The shared cache is expected on every PC; the app keeps asking until
    /// it works. Only IT should switch this off (in settings.json).
    pub shared_cache_enabled: bool,
    /// Keep the background reader running after the window is closed.
    pub keep_indexing_when_closed: bool,
    /// Other programs using more CPU than this (while someone is at the PC)
    /// keep reading at its minimum; it never stops.
    pub busy_cpu_percent: u32,
    /// Rebuild the file-name index of each drive this often.
    pub name_rescan_minutes: u32,
    /// Concurrent folder listings allowed per file server during a deep search.
    pub listings_per_server: u32,
    /// Seconds before a single folder listing is given up and reported.
    pub listing_timeout_secs: u32,
    pub update_channel: UpdateChannel,
    /// Where OCR runs (IT override; normally Auto).
    pub ocr_backend: OcrBackend,
    /// On plotted (text) drawings with no drawing number in their text, read
    /// the title block area with OCR.
    pub ocr_missing_title_block: bool,
    pub projects: Vec<Project>,
    /// Tag parser rules; `None` means the built-in rules.
    pub tag_rules: Option<Vec<TagRule>>,
    /// Title-block rules; `None` means the built-in rules.
    pub field_rules: Option<Vec<FieldRule>>,
    /// Drawing-number prefix -> discipline; `None` means the built-in list.
    pub discipline_prefixes: Option<Vec<(String, String)>>,
    pub revision_order: RevisionOrder,
    pub server: ServerSettings,
    /// Walk only these folders instead of every drive (office server).
    pub index_roots: Option<Vec<String>>,
    /// Finding files is paused on this PC until the user resumes it.
    pub pause_walk: bool,
    /// Reading is paused on this PC until the user resumes it.
    pub pause_reading: bool,
    /// How hard this PC reads (a timed full speed is not saved).
    pub read_speed: crate::protocol::SpeedMode,
    /// File types found by name only, not read inside (empty = read all).
    pub kinds_not_read: Vec<crate::kinds::FileKind>,
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
            shared_cache_unc: None,
            shared_cache_enabled: true,
            keep_indexing_when_closed: true,
            busy_cpu_percent: 60,
            name_rescan_minutes: 360,
            listings_per_server: 4,
            listing_timeout_secs: 20,
            update_channel: UpdateChannel::Stable,
            ocr_backend: OcrBackend::Auto,
            ocr_missing_title_block: true,
            projects: Vec::new(),
            tag_rules: None,
            field_rules: None,
            discipline_prefixes: None,
            revision_order: RevisionOrder::LettersThenNumbers,
            server: ServerSettings::default(),
            index_roots: None,
            pause_walk: false,
            pause_reading: false,
            read_speed: Default::default(),
            kinds_not_read: Vec::new(),
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

/// The last settings saved in full, beside `path`.
fn good_copy(path: &Path) -> PathBuf {
    path.with_file_name("settings.good.json")
}

/// Bring an older settings document up to [`SCHEMA_VERSION`]. Each step only
/// adds or renames keys.
fn migrate(mut doc: Map<String, Value>) -> Map<String, Value> {
    let from = doc.get("schema_version").and_then(Value::as_u64).unwrap_or(0) as u32;
    if from < 1 {
        // v0 was never released; nothing to rename.
    }
    // v1 -> v2 (Sleuth 1.0) only adds keys, all with defaults.
    doc.insert("schema_version".into(), Value::from(SCHEMA_VERSION.max(from)));
    doc
}

impl Settings {
    pub fn tag_rules(&self) -> Vec<TagRule> {
        self.tag_rules.clone().unwrap_or_else(default_tag_rules)
    }
    pub fn field_rules(&self) -> Vec<FieldRule> {
        self.field_rules.clone().unwrap_or_else(default_field_rules)
    }
    pub fn discipline_prefixes(&self) -> Vec<(String, String)> {
        self.discipline_prefixes.clone().unwrap_or_else(default_discipline_prefixes)
    }

    /// Another way to the index folder `dir` when it cannot be reached as
    /// written: the network path learned while it worked, or, for the office
    /// default on U:, its known network path.
    pub fn shared_cache_fallback(&self, dir: &str) -> Option<String> {
        let norm = |d: &str| d.trim().trim_end_matches('\\').to_lowercase();
        let same = self.shared_cache_dir.as_deref().is_some_and(|d| norm(d) == norm(dir));
        if same {
            if let Some(u) = self.shared_cache_unc.as_deref().filter(|u| u.starts_with(r"\\")) {
                return Some(u.to_string());
            }
        }
        (norm(dir) == norm(OFFICE_CACHE_DIR)).then(|| OFFICE_CACHE_UNC.to_string())
    }

    /// Load the settings. A file that is missing, empty or damaged (a PC
    /// switched off while it was written) is replaced by the last good copy
    /// (`settings.good.json`), so the reading folders and everything else
    /// chosen survive restarts, updates and power cuts. Defaults only when
    /// there is no good copy either.
    pub fn load_from(path: &Path) -> anyhow::Result<Settings> {
        if let Ok(Some(s)) = Self::load_one(path, true) {
            return Ok(s);
        }
        if path.exists() {
            // Kept for inspection.
            let _ = std::fs::copy(path, path.with_extension("damaged.json"));
        }
        match Self::load_one(&good_copy(path), false) {
            Ok(Some(s)) => {
                let _ = s.save_to(path);
                Ok(s)
            }
            _ => Ok(Settings::default()),
        }
    }

    /// `None`: no usable settings in this file.
    fn load_one(path: &Path, upgrade: bool) -> anyhow::Result<Option<Settings>> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Notepad and PowerShell 5.1 save UTF-8 with a byte-order mark.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let Ok(doc) = serde_json::from_str::<Map<String, Value>>(text) else { return Ok(None) };
        let old_version = doc.get("schema_version").and_then(Value::as_u64).unwrap_or(0) as u32;
        let migrated = migrate(doc);
        let Ok(settings) = serde_json::from_value::<Settings>(Value::Object(migrated)) else { return Ok(None) };
        if upgrade && old_version < SCHEMA_VERSION {
            let _ = std::fs::copy(path, path.with_file_name(format!("settings.v{old_version}.bak")));
            settings.save_to(path)?;
        }
        Ok(Some(settings))
    }

    /// Flushed to disk before it replaces the old file (a power cut cannot
    /// leave an empty file), and kept as the last good copy too.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = serde_json::to_vec_pretty(self)?;
        let write = |p: &Path| -> std::io::Result<()> {
            let mut f = std::fs::File::create(p)?;
            f.write_all(&body)?;
            f.sync_all()
        };
        let tmp = path.with_extension("json.tmp");
        write(&tmp)?;
        std::fs::rename(&tmp, path)?;
        let good = good_copy(path);
        let gtmp = good.with_extension("json.tmp");
        if write(&gtmp).is_ok() {
            let _ = std::fs::rename(&gtmp, &good);
        }
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
    fn index_folder_fallback() {
        let mut s = Settings::default();
        // the office default on U: falls back to its known network path
        assert_eq!(s.shared_cache_fallback(r"u:\sleuth-cache\").as_deref(), Some(OFFICE_CACHE_UNC));
        assert_eq!(s.shared_cache_fallback(r"P:\Other"), None);
        // a learned path wins, but only for the folder it was learned for
        s.shared_cache_dir = Some(r"P:\Index".into());
        s.shared_cache_unc = Some(r"\\fs1\data\Index".into());
        assert_eq!(s.shared_cache_fallback(r"p:\index").as_deref(), Some(r"\\fs1\data\Index"));
        assert_eq!(s.shared_cache_fallback(r"Q:\Index"), None);
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
    fn file_with_byte_order_mark_loads() {
        let p = tmp("bom");
        std::fs::write(&p, "\u{feff}{\"schema_version\":2,\"ocr_dpi\":400}").unwrap();
        assert_eq!(Settings::load_from(&p).unwrap().ocr_dpi, 400);
    }

    #[test]
    fn damaged_file_falls_back() {
        let p = tmp("damaged");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(Settings::load_from(&p).unwrap(), Settings::default());
    }

    /// A PC switched off while saving left an empty settings.json; the
    /// reading folders came back as none (Nov 2026).
    #[test]
    fn an_emptied_file_comes_back_from_the_good_copy() {
        let p = tmp("emptied").with_file_name("settings.json");
        let _ = std::fs::remove_file(good_copy(&p));
        let s = Settings { content_scopes: vec![r"P:\P12513".into()], ..Default::default() };
        s.save_to(&p).unwrap();
        std::fs::write(&p, "").unwrap();
        assert_eq!(Settings::load_from(&p).unwrap().content_scopes, vec![r"P:\P12513".to_string()]);
        assert_eq!(Settings::load_from(&p).unwrap().content_scopes.len(), 1, "and settings.json is whole again");
    }
}
