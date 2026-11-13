//! The connection card: what desktops need to find and trust this server,
//! left in the office index folder they all use.
//!
//! `sleuth-server.json` holds the address, the CA certificate (public) and,
//! when PCs may connect themselves, the join code. A PC that can open the
//! office index folder reads it, asks `POST /api/v1/join` with the code and
//! gets its own access key: nobody types an address, a certificate file or
//! a key. `sleuth-server-ca.pem` is written beside it for Sleuth 1.1/1.2.
//!
//! The server writes the card when it starts and whenever the window
//! changes something. The service runs as its own account, which may not be
//! allowed to write to the shared folder; the window (run by a person) then
//! writes it, and says so if it cannot either.

use crate::config::{self, Config};
use crate::tls;
use anyhow::{Context, Result};
use sleuth_core::protocol::OfficeServerCard;
use std::path::{Path, PathBuf};

fn listen_port(c: &Config) -> u16 {
    c.listen.parse::<std::net::SocketAddr>().map(|a| a.port()).unwrap_or(8455)
}

/// The card for this server as it stands now.
pub fn card(data: &Path, c: &Config) -> Result<OfficeServerCard> {
    let port = listen_port(c);
    let ca_pem = std::fs::read_to_string(data.join(tls::CA_CERT)).context("reading the CA certificate")?;
    Ok(OfficeServerCard {
        address: format!("https://{}:{port}", tls::computer_name().to_lowercase()),
        addresses: tls::local_ipv4().iter().map(|ip| format!("https://{ip}:{port}")).collect(),
        pc: tls::computer_name(),
        at: sleuth_engine::service::now(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        ca_sha256: tls::fingerprint(&ca_pem).unwrap_or_default(),
        ca_pem,
        join_code: if c.self_registration { c.join_code.clone() } else { String::new() },
    })
}

/// Give the configuration a join code when PCs may connect themselves and
/// it has none yet. Returns the configuration as saved.
pub fn ensure_join_code(data: &Path) -> Result<Config> {
    let mut c = Config::load(data)?;
    if c.self_registration && c.join_code.is_empty() {
        c.join_code = config::new_join_code()?;
        c.save(data)?;
    }
    Ok(c)
}

/// Write the card and the CA file into the office index folder. Returns
/// the card's path, or `None` when no office index folder is set.
pub fn publish(data: &Path, c: &Config) -> Result<Option<PathBuf>> {
    let Some(dir) = c.shared_cache_dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) else { return Ok(None) };
    let card = card(data, c)?;
    let path = Path::new(dir).join(sleuth_engine::remote::SERVER_CARD_FILE);
    let tmp = Path::new(dir).join(format!("{}.tmp", sleuth_engine::remote::SERVER_CARD_FILE));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {dir}"))?;
    std::fs::write(&tmp, serde_json::to_string_pretty(&card)?).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))?;
    tls::publish_ca(data, dir)?;
    Ok(Some(path))
}

/// What the card in the office index folder says now, if it can be read.
pub fn published(c: &Config) -> Option<OfficeServerCard> {
    let dir = c.shared_cache_dir.as_deref()?;
    let text = std::fs::read_to_string(Path::new(dir).join(sleuth_engine::remote::SERVER_CARD_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}
