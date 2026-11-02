//! The server's own certificate authority and HTTPS certificate.
//!
//! - `init` makes a private CA (10 years) and a server certificate (2 years)
//!   signed by it. Desktops trust only that CA, found as
//!   `sleuth-server-ca.pem` in the shared cache folder.
//! - The server certificate names this computer, `localhost`, `127.0.0.1`,
//!   every IPv4 address this computer's name resolves to, and the extra
//!   `hostnames` from `server.json`.
//! - `renew-cert` signs a new server certificate with the same CA, so
//!   desktops need nothing new. An existing CA is never replaced.
//! - Keys are written next to the certificates in the data folder; the
//!   folder's permissions are what protect them (see `service install`).

use anyhow::{Context, Result, anyhow, bail};
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};

pub const CA_CERT: &str = "ca.pem";
pub const CA_KEY: &str = "ca.key";
pub const SERVER_CERT: &str = "server.pem";
pub const SERVER_KEY: &str = "server.key";
const CA_DAYS: i64 = 3650;
const SERVER_DAYS: i64 = 730;

pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
}

/// IPv4 addresses of this computer, as its own name resolves.
pub fn local_ipv4() -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = (computer_name().as_str(), 0)
        .to_socket_addrs()
        .map(|it| {
            it.filter_map(|a| match a.ip() {
                IpAddr::V4(v) if !v.is_unspecified() => Some(v),
                _ => None,
            })
            .collect()
        })
        .unwrap_or_default();
    out.sort();
    out.dedup();
    out
}

/// Names and addresses for the server certificate.
pub fn subject_names(hostnames: &[String]) -> (Vec<String>, Vec<IpAddr>) {
    let mut dns: Vec<String> = Vec::new();
    let mut ips: Vec<IpAddr> = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    let host = computer_name();
    let domain = std::env::var("USERDNSDOMAIN").ok().filter(|d| !d.trim().is_empty());
    let mut names = vec![host.clone(), "localhost".to_string()];
    if let Some(d) = domain {
        names.push(format!("{host}.{d}"));
    }
    names.extend(hostnames.iter().cloned());
    for n in names {
        let n = n.trim().to_lowercase();
        if n.is_empty() {
            continue;
        }
        match n.parse::<IpAddr>() {
            Ok(ip) => {
                if !ips.contains(&ip) {
                    ips.push(ip);
                }
            }
            Err(_) => {
                if !dns.contains(&n) {
                    dns.push(n);
                }
            }
        }
    }
    for v in local_ipv4() {
        let ip = IpAddr::V4(v);
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    (dns, ips)
}

fn write_new(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

fn ca_params() -> Result<CertificateParams> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, format!("Sleuth office server CA ({})", computer_name()));
    dn.push(DnType::OrganizationName, "Sleuth");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    let now = OffsetDateTime::now_utc();
    p.not_before = now - Duration::days(1);
    p.not_after = now + Duration::days(CA_DAYS);
    Ok(p)
}

/// Make the CA if there is none. Returns true when one was made.
pub fn ensure_ca(dir: &Path) -> Result<bool> {
    let (cert, key) = (dir.join(CA_CERT), dir.join(CA_KEY));
    match (cert.is_file(), key.is_file()) {
        (true, true) => return Ok(false),
        (false, false) => {}
        _ => bail!("only one of {CA_CERT} and {CA_KEY} is in {}; restore both from the backup (making a new CA would cut off every desktop)", dir.display()),
    }
    let k = KeyPair::generate()?;
    let c = ca_params()?.self_signed(&k)?;
    write_new(&key, &k.serialize_pem())?;
    write_new(&cert, &c.pem())?;
    Ok(true)
}

/// Sign a new server certificate with the CA in `dir`.
pub fn issue_server_cert(dir: &Path, hostnames: &[String]) -> Result<(Vec<String>, Vec<IpAddr>)> {
    let ca_pem = std::fs::read_to_string(dir.join(CA_CERT)).context("reading the CA certificate")?;
    let ca_key = KeyPair::from_pem(&std::fs::read_to_string(dir.join(CA_KEY)).context("reading the CA key")?)?;
    let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key).map_err(|e| anyhow!("the CA certificate cannot be used: {e}"))?;
    let (dns, ips) = subject_names(hostnames);
    let mut p = CertificateParams::new(dns.clone())?;
    for ip in &ips {
        p.subject_alt_names.push(SanType::IpAddress(*ip));
    }
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, computer_name());
    dn.push(DnType::OrganizationName, "Sleuth");
    p.distinguished_name = dn;
    p.is_ca = IsCa::ExplicitNoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    p.use_authority_key_identifier_extension = true;
    let now = OffsetDateTime::now_utc();
    p.not_before = now - Duration::days(1);
    p.not_after = now + Duration::days(SERVER_DAYS);
    let k = KeyPair::generate()?;
    let c = p.signed_by(&k, &issuer)?;
    write_new(&dir.join(SERVER_KEY), &k.serialize_pem())?;
    write_new(&dir.join(SERVER_CERT), &c.pem())?;
    Ok((dns, ips))
}

/// SHA-256 of a PEM certificate's DER, as `AA:BB:...`.
pub fn fingerprint(pem: &str) -> Result<String> {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|e| anyhow!("not a PEM certificate: {e}"))?;
    let d = Sha256::digest(&p.contents);
    Ok(d.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"))
}

/// Unix time a PEM certificate stops being valid.
pub fn expires(path: &Path) -> Result<i64> {
    let pem = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (_, p) = x509_parser::pem::parse_x509_pem(&pem).map_err(|e| anyhow!("{} is not a PEM certificate: {e}", path.display()))?;
    let c = p.parse_x509().map_err(|e| anyhow!("{} is not a certificate: {e}", path.display()))?;
    Ok(c.validity().not_after.timestamp())
}

/// Copy the CA certificate where desktops look for it. Returns the copy.
pub fn publish_ca(dir: &Path, shared_cache: &str) -> Result<PathBuf> {
    let target = Path::new(shared_cache.trim()).join(sleuth_engine::remote::CA_FILE_NAME);
    std::fs::create_dir_all(Path::new(shared_cache.trim())).with_context(|| format!("cannot create {shared_cache}"))?;
    std::fs::copy(dir.join(CA_CERT), &target).with_context(|| format!("cannot copy the CA certificate to {}", target.display()))?;
    Ok(target)
}

/// rustls settings for serving `server.pem` (ring crypto, HTTP/1.1 and h2).
pub fn server_config(dir: &Path) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(dir.join(SERVER_CERT))
        .with_context(|| format!("reading {SERVER_CERT}"))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("{SERVER_CERT} is damaged"))?;
    let key = PrivateKeyDer::from_pem_file(dir.join(SERVER_KEY)).with_context(|| format!("reading {SERVER_KEY}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the server certificate and key do not belong together")?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_signs_and_renews() {
        let dir = std::env::temp_dir().join(format!("sleuth-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(ensure_ca(&dir).unwrap());
        assert!(!ensure_ca(&dir).unwrap());
        let ca = std::fs::read_to_string(dir.join(CA_CERT)).unwrap();
        let (dns, ips) = issue_server_cert(&dir, &["sleuth.example.local".into(), "10.9.8.7".into()]).unwrap();
        assert!(dns.contains(&"localhost".to_string()) && dns.contains(&"sleuth.example.local".to_string()));
        assert!(ips.contains(&"127.0.0.1".parse().unwrap()) && ips.contains(&"10.9.8.7".parse().unwrap()));
        let first = std::fs::read_to_string(dir.join(SERVER_CERT)).unwrap();
        issue_server_cert(&dir, &[]).unwrap();
        assert_ne!(first, std::fs::read_to_string(dir.join(SERVER_CERT)).unwrap());
        assert_eq!(ca, std::fs::read_to_string(dir.join(CA_CERT)).unwrap(), "renewing keeps the CA");
        let left = expires(&dir.join(SERVER_CERT)).unwrap() - OffsetDateTime::now_utc().unix_timestamp();
        assert!((728 * 86_400..731 * 86_400).contains(&left));
        assert!(server_config(&dir).is_ok());
        assert_eq!(fingerprint(&ca).unwrap().len(), 32 * 3 - 1);
        std::fs::remove_file(dir.join(CA_KEY)).unwrap();
        assert!(ensure_ca(&dir).is_err(), "a half-missing CA is never replaced");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
