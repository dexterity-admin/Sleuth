//! The desktop side of the optional office server.
//!
//! - HTTPS only. The server's own certificate authority is trusted, nothing
//!   else: the CA file is read from settings, or `sleuth-server-ca.pem` in the
//!   shared cache folder (where the server's setup puts it).
//! - The access key is stored encrypted for the Windows user (DPAPI) and sent
//!   as a bearer token; it never appears in logs.
//! - Every call has a short timeout; a server that does not answer makes the
//!   office part of a search "unavailable", never slow.
//! - The API is versioned (`/api/v1`); a server with another major version is
//!   reported, not used.

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sleuth_core::protocol::{Hit, Occurrence, Origin, Related, RevisionGroup, TagChange, TagSummary};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const API_MAJOR: u32 = 1;
pub const CA_FILE_NAME: &str = "sleuth-server-ca.pem";
/// The server's connection card in the office index folder (address, PC).
pub const SERVER_CARD_FILE: &str = "sleuth-server.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub api_version: u32,
    pub server_version: String,
    pub user: String,
    pub role: String,
    pub projects: Vec<String>,
    pub roots: Vec<String>,
    /// Still reading its first pass over the shared folders.
    pub syncing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchReply {
    pub hits: Vec<Hit>,
    pub more: bool,
    pub millis: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageReply {
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
    pub pages: u32,
    pub boxes: Vec<sleuth_core::protocol::NormBox>,
    pub source: sleuth_core::protocol::TextSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocLookup {
    pub id: i64,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: String,
}

/// Where to find the server's CA certificate.
pub fn ca_path(ca_file: &str, shared_cache: Option<&str>, drives: &sleuth_core::paths::DriveMap) -> Option<PathBuf> {
    if !ca_file.trim().is_empty() {
        return Some(PathBuf::from(drives.to_unc(ca_file.trim())));
    }
    shared_cache.map(|d| Path::new(&drives.to_unc(d)).join(CA_FILE_NAME))
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Client {
    pub fn new(url: &str, token: &str, ca_pem: &[u8]) -> Result<Client> {
        let base = url.trim().trim_end_matches('/').to_string();
        if !base.starts_with("https://") {
            bail!("the server address must start with https://");
        }
        if token.trim().is_empty() {
            bail!("an access key is needed");
        }
        let cert = ureq::tls::Certificate::from_pem(ca_pem).map_err(|e| anyhow!("the server certificate file is not a PEM certificate: {e}"))?;
        let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::Specific(std::sync::Arc::new(vec![cert]))).build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_connect(Some(Duration::from_secs(4)))
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Client { agent, base, token: token.trim().to_string() })
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}/api/v1{path}", self.base);
        let mut resp = self.agent.get(&url).header("Authorization", &format!("Bearer {}", self.token)).call().map_err(|e| anyhow!("{}", describe(&e)))?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().context("reading the server's answer")?;
        if status >= 400 {
            let e: Option<ApiError> = serde_json::from_str(&body).ok();
            bail!("{}", e.map(|e| e.message).unwrap_or_else(|| format!("the server answered {status}")));
        }
        serde_json::from_str(&body).context("the server's answer was not understood")
    }

    pub fn capabilities(&self) -> Result<Capabilities> {
        let c: Capabilities = self.get("/capabilities")?;
        if c.api_version != API_MAJOR {
            bail!("the office server speaks API version {}, this Sleuth speaks {API_MAJOR}; update one of them", c.api_version);
        }
        Ok(c)
    }

    pub fn search(&self, q: &str, project: Option<&str>, limit: usize, offset: usize) -> Result<SearchReply> {
        let mut path = format!("/search?q={}&limit={limit}&offset={offset}", enc(q));
        if let Some(p) = project {
            path.push_str(&format!("&project={}", enc(p)));
        }
        let mut r: SearchReply = self.get(&path)?;
        for h in &mut r.hits {
            h.origin = Origin::Office;
        }
        Ok(r)
    }

    pub fn lookup(&self, path: &str) -> Result<DocLookup> {
        self.get(&format!("/documents?path={}", enc(path)))
    }

    pub fn page(&self, doc: i64, page: u32, q: &str, tag: Option<&str>, max_px: u32) -> Result<PageReply> {
        let mut p = format!("/documents/{doc}/pages/{page}?q={}&max_px={max_px}", enc(q));
        if let Some(t) = tag {
            p.push_str(&format!("&tag={}", enc(t)));
        }
        self.get(&p)
    }

    pub fn tags(&self, category: Option<&str>, contains: Option<&str>, project: Option<&str>, limit: usize, offset: usize) -> Result<(Vec<TagSummary>, u64)> {
        let mut p = format!("/tags?limit={limit}&offset={offset}");
        for (k, v) in [("category", category), ("contains", contains), ("project", project)] {
            if let Some(v) = v {
                p.push_str(&format!("&{k}={}", enc(v)));
            }
        }
        #[derive(Deserialize)]
        struct R {
            tags: Vec<TagSummary>,
            total: u64,
        }
        let r: R = self.get(&p)?;
        Ok((r.tags, r.total))
    }

    pub fn occurrences(&self, norm: &str, project: Option<&str>) -> Result<Vec<Occurrence>> {
        let mut p = format!("/tags/{}/occurrences", enc(norm));
        if let Some(v) = project {
            p.push_str(&format!("?project={}", enc(v)));
        }
        self.get(&p)
    }

    pub fn revisions(&self, contains: Option<&str>, project: Option<&str>, limit: usize) -> Result<Vec<RevisionGroup>> {
        let mut p = format!("/revisions?limit={limit}");
        for (k, v) in [("contains", contains), ("project", project)] {
            if let Some(v) = v {
                p.push_str(&format!("&{k}={}", enc(v)));
            }
        }
        self.get(&p)
    }

    pub fn compare(&self, old: i64, new: i64) -> Result<CompareReply> {
        self.get(&format!("/compare?old={old}&new={new}"))
    }

    pub fn related(&self, doc: i64) -> Result<Vec<Related>> {
        self.get(&format!("/documents/{doc}/related"))
    }

    pub fn document(&self, doc: i64) -> Result<DocReply> {
        self.get(&format!("/documents/{doc}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareReply {
    pub old: sleuth_core::protocol::DrawingMeta,
    pub new: sleuth_core::protocol::DrawingMeta,
    pub changes: Vec<TagChange>,
    pub unchanged: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocReply {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub metadata: sleuth_core::protocol::DrawingMeta,
    pub pages: Vec<sleuth_core::protocol::PageInfo>,
    pub provenance: sleuth_core::protocol::Provenance,
    pub tags: u32,
}

/// What a PC sends to get its own access key (`POST /api/v1/join`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    /// The join code from the server's card in the office index folder.
    pub code: String,
    pub pc: String,
    pub user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinReply {
    /// The PC's new access key (shown nowhere; stored encrypted).
    pub key: String,
    /// The name the server knows this PC by, e.g. `DDS-030\vikram`.
    pub name: String,
    pub projects: Vec<String>,
    pub server_version: String,
}

/// Ask the server at `url` (trusted through `ca_pem`) for this PC's key.
pub fn join(url: &str, ca_pem: &[u8], req: &JoinRequest) -> Result<JoinReply> {
    let base = url.trim().trim_end_matches('/');
    if !base.starts_with("https://") {
        bail!("the server address must start with https://");
    }
    let cert = ureq::tls::Certificate::from_pem(ca_pem).map_err(|e| anyhow!("the server certificate is not a PEM certificate: {e}"))?;
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::Specific(std::sync::Arc::new(vec![cert]))).build();
    let agent: ureq::Agent = ureq::Agent::config_builder().tls_config(tls).timeout_connect(Some(Duration::from_secs(4))).timeout_global(Some(Duration::from_secs(30))).http_status_as_error(false).build().into();
    let mut resp = agent.post(&format!("{base}/api/v1/join")).header("Content-Type", "application/json").send(serde_json::to_string(req)?).map_err(|e| anyhow!("{}", describe(&e)))?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().context("reading the server's answer")?;
    if status == 404 {
        bail!("the office server is older than this Sleuth and cannot connect PCs by itself; update the server, or type an access key");
    }
    if status >= 400 {
        let e: Option<ApiError> = serde_json::from_str(&body).ok();
        bail!("{}", e.map(|e| e.message).unwrap_or_else(|| format!("the server answered {status}")));
    }
    serde_json::from_str(&body).context("the server's answer was not understood")
}

fn describe(e: &ureq::Error) -> String {
    let s = e.to_string();
    if s.contains("certificate") || s.contains("Certificate") || s.contains("UnknownIssuer") {
        "the server's certificate is not the one Sleuth trusts (check the certificate file)".into()
    } else if s.contains("timed out") || s.contains("Timeout") {
        "the office server did not answer in time".into()
    } else if s.contains("onnect") || s.contains("resolve") || s.contains("dns") {
        format!("the office server cannot be reached ({s})")
    } else {
        s
    }
}

// ---- access key storage (DPAPI, per Windows user) --------------------------

#[cfg(windows)]
pub fn protect(secret: &str) -> Result<String> {
    use base64::Engine as _;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData};
    let mut data = secret.as_bytes().to_vec();
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_mut_ptr() };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out).map_err(|e| anyhow!("could not protect the access key: {e}"))?;
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}

#[cfg(windows)]
pub fn unprotect(stored: &str) -> Result<String> {
    use base64::Engine as _;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData};
    let mut data = base64::engine::general_purpose::STANDARD.decode(stored.trim()).context("stored access key is damaged")?;
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_mut_ptr() };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .map_err(|_| anyhow!("the stored access key belongs to another Windows user or PC; enter it again"))?;
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
        String::from_utf8(bytes).context("stored access key is damaged")
    }
}

#[cfg(not(windows))]
pub fn protect(secret: &str) -> Result<String> {
    Ok(secret.to_string())
}

#[cfg(not(windows))]
pub fn unprotect(stored: &str) -> Result<String> {
    Ok(stored.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_key_round_trip() {
        let p = protect("sk-test-123").unwrap();
        assert_ne!(p, "sk-test-123");
        assert_eq!(unprotect(&p).unwrap(), "sk-test-123");
        assert!(unprotect("bm90IGEga2V5").is_err());
    }

    #[test]
    fn only_https_with_a_key() {
        assert!(Client::new("http://srv:8455", "k", b"x").is_err());
        assert!(Client::new("https://srv:8455", " ", b"x").is_err());
        assert!(Client::new("https://srv:8455", "k", b"not pem").is_err());
    }

    #[test]
    fn query_encoding() {
        assert_eq!(enc("10\"-P 1001&x"), "10%22-P%201001%26x");
    }
}
