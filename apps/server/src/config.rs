//! `server.json`: what the office server indexes and who may see it.
//!
//! - `projects` name sets of folders (UNC for shares). The engine indexes the
//!   union of all project folders and reads every PDF in them.
//! - `users` hold only the SHA-256 of their access key, never the key. A key
//!   is shown once, when `add-user` makes it.
//! - A user sees the folders of their projects (`["*"]` = every project) and
//!   nothing else; [`path_allowed`] is the one test every path goes through.
//! - `reader` is passed to the engine's settings (OCR quality, resolution,
//!   rescan interval ...). Keys the server sets itself are refused there.
//!
//! The file is checked on every load; an error names the entry at fault and
//! never prints a key hash.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sleuth_core::settings::{Project, Settings};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "server.json";
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8455";
/// Prefix of generated access keys, so a key pasted somewhere is recognisable.
pub const KEY_PREFIX: &str = "slk_";
/// Engine settings the server derives from its own configuration.
const OWNED_KEYS: [&str; 6] = ["index_roots", "content_scopes", "projects", "shared_cache_dir", "shared_cache_enabled", "server"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserCfg {
    pub name: String,
    pub token_sha256: String,
    pub role: Role,
    /// Project names, or `["*"]` for all of them.
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Extra names put in the server certificate (DNS aliases).
    #[serde(default)]
    pub hostnames: Vec<String>,
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub users: Vec<UserCfg>,
    /// Shared reading cache, also where the CA file is published for desktops.
    #[serde(default)]
    pub shared_cache_dir: Option<String>,
    /// Passed through to the engine's settings.
    #[serde(default)]
    pub reader: Map<String, Value>,
}

fn default_listen() -> String {
    DEFAULT_LISTEN.into()
}

impl Default for Config {
    fn default() -> Self {
        Config { listen: default_listen(), hostnames: Vec::new(), projects: Vec::new(), users: Vec::new(), shared_cache_dir: None, reader: Map::new() }
    }
}

/// `%ProgramData%\Sleuth Server`.
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData")).join("Sleuth Server")
}

pub fn config_path(data: &Path) -> PathBuf {
    data.join(CONFIG_FILE)
}

/// Lower case, `/` as `\`, no trailing `\`.
fn norm(p: &str) -> String {
    p.trim().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// True when `path` is one of `roots` or inside one, compared
/// case-insensitively by whole folder names: `\\srv\proj` allows
/// `\\srv\proj\a.pdf` but not `\\srv\project2\a.pdf`. Paths with `.` or `..`
/// components are never allowed.
pub fn path_allowed(path: &str, roots: &[String]) -> bool {
    let p = norm(path);
    if p.is_empty() || p.split('\\').any(|c| c == "." || c == "..") {
        return false;
    }
    roots.iter().map(|r| norm(r)).any(|r| !r.is_empty() && (p == r || (p.len() > r.len() && p.starts_with(&r) && p.as_bytes()[r.len()] == b'\\')))
}

/// The folders of `wanted` that lie inside `allowed`.
pub fn roots_within(wanted: &[String], allowed: &[String]) -> Vec<String> {
    wanted.iter().filter(|r| path_allowed(r, allowed)).cloned().collect()
}

pub fn hash_key(key: &str) -> String {
    let d = Sha256::digest(key.trim().as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new random access key (256 bits).
pub fn new_key() -> Result<String> {
    use base64::Engine as _;
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no random numbers from Windows: {e}"))?;
    Ok(format!("{KEY_PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)))
}

/// Compare without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A path the server can walk on its own: `\\server\share\...` or `X:\...`.
fn absolute_folder(p: &str) -> bool {
    let p = p.replace('/', "\\");
    let unc = p.starts_with("\\\\") && p[2..].split('\\').filter(|c| !c.is_empty()).count() >= 2;
    let b = p.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    (unc || drive) && !p.split('\\').any(|c| c == "." || c == "..")
}

impl Config {
    pub fn load(data: &Path) -> Result<Config> {
        let path = config_path(data);
        let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {} (run `sleuth-server init` first)", path.display()))?;
        let c: Config = serde_json::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
        c.validate().with_context(|| format!("{} is not valid", path.display()))?;
        Ok(c)
    }

    /// Write through a temporary file, so a crash never leaves half a file.
    pub fn save(&self, data: &Path) -> Result<()> {
        self.validate()?;
        std::fs::create_dir_all(data)?;
        let path = config_path(data);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        self.listen.parse::<SocketAddr>().with_context(|| format!("listen \"{}\" is not an address:port such as {DEFAULT_LISTEN}", self.listen))?;
        let mut names = HashSet::new();
        for p in &self.projects {
            let n = p.name.trim();
            if n.is_empty() || n == "*" || n.contains([',', '=']) {
                bail!("project name \"{}\" is not allowed (empty, \"*\", or contains , or =)", p.name);
            }
            if !names.insert(n.to_lowercase()) {
                bail!("project \"{n}\" is listed twice");
            }
            if p.roots.is_empty() {
                bail!("project \"{n}\" has no folders");
            }
            for r in &p.roots {
                if !absolute_folder(r) {
                    bail!("project \"{n}\": \"{r}\" is not a full folder path (\\\\server\\share\\... or X:\\...)");
                }
            }
        }
        let mut users = HashSet::new();
        let mut hashes = HashSet::new();
        for u in &self.users {
            let n = u.name.trim();
            if n.is_empty() || n.chars().any(|c| c.is_control()) {
                bail!("a user has an empty or unprintable name");
            }
            if !users.insert(n.to_lowercase()) {
                bail!("user \"{n}\" is listed twice");
            }
            if u.token_sha256.len() != 64 || !u.token_sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                bail!("user \"{n}\": token_sha256 must be 64 lower-case hex digits");
            }
            if !hashes.insert(u.token_sha256.clone()) {
                bail!("user \"{n}\" has the same access key as another user");
            }
            if u.projects.is_empty() {
                bail!("user \"{n}\" has no projects (use [\"*\"] for all)");
            }
            for p in &u.projects {
                if p != "*" && !names.contains(&p.trim().to_lowercase()) {
                    bail!("user \"{n}\": no project called \"{p}\"");
                }
            }
        }
        for k in self.reader.keys() {
            if OWNED_KEYS.contains(&k.as_str()) {
                bail!("reader.{k} is set by the server itself; use projects / shared_cache_dir instead");
            }
        }
        self.engine_settings().context("reader settings")?;
        Ok(())
    }

    /// The user whose key hashes to `key`'s hash.
    pub fn user_by_key(&self, key: &str) -> Option<&UserCfg> {
        let h = hash_key(key);
        // Look at every user, so the time taken does not depend on which one matched.
        let mut found = None;
        for u in &self.users {
            if same(&u.token_sha256, &h) && found.is_none() {
                found = Some(u);
            }
        }
        found
    }

    pub fn user(&self, name: &str) -> Option<&UserCfg> {
        self.users.iter().find(|u| u.name.eq_ignore_ascii_case(name.trim()))
    }

    pub fn project(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name.eq_ignore_ascii_case(name.trim()))
    }

    /// The projects a user may see, in configuration order.
    pub fn user_projects(&self, u: &UserCfg) -> Vec<&Project> {
        let all = u.projects.iter().any(|p| p == "*");
        self.projects.iter().filter(|p| all || u.projects.iter().any(|x| x.eq_ignore_ascii_case(&p.name))).collect()
    }

    /// Every folder a user may see.
    pub fn user_roots(&self, u: &UserCfg) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in self.user_projects(u) {
            for r in &p.roots {
                if !out.iter().any(|x| norm(x) == norm(r)) {
                    out.push(r.clone());
                }
            }
        }
        out
    }

    /// Every project folder, once.
    pub fn all_roots(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in self.projects.iter().flat_map(|p| &p.roots) {
            if !out.iter().any(|x| norm(x) == norm(r)) {
                out.push(r.clone());
            }
        }
        out
    }

    /// The engine's settings: defaults, then `reader`, then the server's own.
    pub fn engine_settings(&self) -> Result<Settings> {
        let mut doc = match serde_json::to_value(Settings::default())? {
            Value::Object(m) => m,
            _ => bail!("settings are not an object"),
        };
        for (k, v) in &self.reader {
            doc.insert(k.clone(), v.clone());
        }
        let mut s: Settings = serde_json::from_value(Value::Object(doc))?;
        let roots = self.all_roots();
        s.index_roots = Some(roots.clone());
        s.content_scopes = roots;
        s.projects = self.projects.clone();
        s.shared_cache_dir = self.shared_cache_dir.clone().filter(|d| !d.trim().is_empty());
        s.shared_cache_enabled = s.shared_cache_dir.is_some();
        s.server = Default::default();
        Ok(s)
    }
}

/// `NAME=\\srv\share[,\\srv\other]` from the command line.
pub fn parse_project(arg: &str) -> Result<Project> {
    let (name, roots) = arg.split_once('=').with_context(|| format!("\"{arg}\" should be NAME=\\\\server\\share[,more]"))?;
    let roots: Vec<String> = roots.split(',').map(|r| r.trim().trim_end_matches(['\\', '/']).to_string()).filter(|r| !r.is_empty()).collect();
    // `X:` alone means the drive root.
    let roots = roots.into_iter().map(|r| if r.len() == 2 && r.ends_with(':') { format!("{r}\\") } else { r }).collect();
    Ok(Project { name: name.trim().to_string(), roots })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn whole_folders_only() {
        let r = roots(&[r"\\srv\proj"]);
        assert!(path_allowed(r"\\srv\proj", &r));
        assert!(path_allowed(r"\\srv\proj\", &r));
        assert!(path_allowed(r"\\srv\proj\Area 1\ISO-1001.pdf", &r));
        assert!(!path_allowed(r"\\srv\project2\a.pdf", &r));
        assert!(!path_allowed(r"\\srv\proj2", &r));
        assert!(!path_allowed(r"\\srv\pro", &r));
        assert!(!path_allowed(r"\\srv\other\proj\a.pdf", &r));
        assert!(!path_allowed("", &r));
        assert!(!path_allowed(r"\\srv\proj\a.pdf", &[]));
    }

    #[test]
    fn unc_case_and_separators() {
        let r = roots(&[r"\\FileSrv\Projects\P12513\"]);
        assert!(path_allowed(r"\\filesrv\projects\p12513\ISO\x.PDF", &r));
        assert!(path_allowed("//FILESRV/Projects/P12513/x.pdf", &r));
        assert!(!path_allowed(r"\\filesrv\projects\p12513x\a.pdf", &r));
        assert!(!path_allowed(r"\\filesrv\projects\p12513\..\other\a.pdf", &r));
        let d = roots(&[r"D:\"]);
        assert!(path_allowed(r"d:\anything\a.pdf", &d));
        assert!(!path_allowed(r"E:\a.pdf", &d));
    }

    #[test]
    fn project_roots_inside_user_roots() {
        let user = roots(&[r"\\srv\proj"]);
        let wanted = roots(&[r"\\srv\proj\area1", r"\\srv\project2"]);
        assert_eq!(roots_within(&wanted, &user), roots(&[r"\\srv\proj\area1"]));
    }

    fn sample() -> Config {
        Config {
            projects: vec![Project { name: "P1".into(), roots: roots(&[r"\\srv\proj"]) }, Project { name: "P2".into(), roots: roots(&[r"\\srv\project2"]) }],
            users: vec![
                UserCfg { name: "ana".into(), token_sha256: hash_key("slk_a"), role: Role::User, projects: vec!["p1".into()] },
                UserCfg { name: "it".into(), token_sha256: hash_key("slk_b"), role: Role::Admin, projects: vec!["*".into()] },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn keys_are_hashed_and_found() {
        let c = sample();
        assert_eq!(hash_key("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hash_key(" abc\n"), hash_key("abc"));
        assert_eq!(c.user_by_key("slk_a").map(|u| u.name.as_str()), Some("ana"));
        assert_eq!(c.user_by_key("slk_b").map(|u| u.name.as_str()), Some("it"));
        assert!(c.user_by_key("slk_c").is_none());
        assert!(c.user_by_key("").is_none());
        let k = new_key().unwrap();
        assert!(k.starts_with(KEY_PREFIX) && k.len() > 40);
        assert_ne!(k, new_key().unwrap());
        // the stored form never contains the key
        assert!(!serde_json::to_string(&c).unwrap().contains("slk_a"));
    }

    #[test]
    fn user_roots_follow_projects() {
        let c = sample();
        assert_eq!(c.user_roots(c.user("ANA").unwrap()), roots(&[r"\\srv\proj"]));
        assert_eq!(c.user_roots(c.user("it").unwrap()), roots(&[r"\\srv\proj", r"\\srv\project2"]));
        let s = c.engine_settings().unwrap();
        assert_eq!(s.index_roots.unwrap(), roots(&[r"\\srv\proj", r"\\srv\project2"]));
        assert_eq!(s.content_scopes.len(), 2);
        assert!(!s.shared_cache_enabled);
    }

    #[test]
    fn validation_names_the_fault() {
        assert!(sample().validate().is_ok());
        let bad = |f: &dyn Fn(&mut Config)| {
            let mut c = sample();
            f(&mut c);
            format!("{:#}", c.validate().unwrap_err())
        };
        assert!(bad(&|c| c.listen = "8455".into()).contains("listen"));
        assert!(bad(&|c| c.projects[1].name = "p1".into()).contains("twice"));
        assert!(bad(&|c| c.projects[0].roots = roots(&["proj"])).contains("full folder path"));
        assert!(bad(&|c| c.projects[0].roots = roots(&[r"\\srv"])).contains("full folder path"));
        assert!(bad(&|c| c.projects[0].roots.clear()).contains("no folders"));
        assert!(bad(&|c| c.users[0].projects = vec!["P9".into()]).contains("no project called"));
        assert!(bad(&|c| c.users[0].token_sha256 = "abc".into()).contains("64"));
        assert!(bad(&|c| c.users[1].token_sha256 = c.users[0].token_sha256.clone()).contains("same access key"));
        assert!(bad(&|c| c.users[1].name = "Ana".into()).contains("twice"));
        assert!(bad(&|c| { c.reader.insert("index_roots".into(), Value::Null); }).contains("set by the server"));
        assert!(bad(&|c| { c.reader.insert("ocr_dpi".into(), Value::String("high".into())); }).contains("reader"));
        // the hash never appears in an error
        let e = bad(&|c| c.users[1].token_sha256 = c.users[0].token_sha256.clone());
        assert!(!e.contains(&hash_key("slk_a")));
        // a role that does not exist is refused when read
        let text = serde_json::to_string(&sample()).unwrap().replace("\"admin\"", "\"root\"");
        assert!(serde_json::from_str::<Config>(&text).is_err());
    }

    #[test]
    fn project_arguments() {
        let p = parse_project(r"P12513=\\srv\proj\, \\srv\proj-b").unwrap();
        assert_eq!(p.name, "P12513");
        assert_eq!(p.roots, roots(&[r"\\srv\proj", r"\\srv\proj-b"]));
        assert_eq!(parse_project("L=D:").unwrap().roots, roots(&[r"D:\"]));
        assert!(parse_project(r"\\srv\proj").is_err());
    }
}
