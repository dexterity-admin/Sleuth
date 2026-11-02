//! Sleuth office server: the same engine as the desktop worker, indexing the
//! project folders on the file server once for the whole office, behind a
//! versioned, authenticated HTTPS API (`/api/v1`).
//!
//! Optional: desktops work on their own without it and show office results
//! as unavailable when it cannot be reached.
//!
//! Data folder (default `%ProgramData%\Sleuth Server`): `server.json`,
//! `audit.db`, the certificates, and the engine's own folders `data\` (index,
//! logs) and `config\`, because `SLEUTH_DATA_DIR` points the engine there.
//!
//! `server.json` is checked every few seconds while running: users and keys
//! take effect at once; changed project folders are handed to the engine,
//! which walks them again. A file that fails validation is ignored (and
//! logged) and the running configuration kept.

pub mod api;
pub mod audit;
pub mod config;
pub mod tls;

#[cfg(windows)]
pub mod winsvc;

use anyhow::{Context, Result, bail};
use api::AppState;
use config::Config;
use sleuth_engine::service::Engine;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

/// Point the engine at the server's data folder. Call before any thread or
/// runtime starts (changing the environment is not thread-safe).
pub fn use_data_dir(data: &Path) {
    // SAFETY: called from `main` (or a test) before other threads exist.
    unsafe { std::env::set_var("SLEUTH_DATA_DIR", data) };
}

fn check_data_dir(data: &Path) -> Result<()> {
    match std::env::var_os("SLEUTH_DATA_DIR") {
        Some(d) if Path::new(&d) == data => Ok(()),
        _ => bail!("SLEUTH_DATA_DIR must be {} before the server starts (call use_data_dir)", data.display()),
    }
}

/// A server that is listening.
pub struct Running {
    pub addr: SocketAddr,
    pub state: Arc<AppState>,
    handle: axum_server::Handle,
    served: tokio::task::JoinHandle<std::io::Result<()>>,
    watcher: tokio::task::JoinHandle<()>,
}

/// Start the engine and the HTTPS listener. `listen` overrides `server.json`
/// (tests use `127.0.0.1:0`).
pub async fn start(data: &Path, listen: Option<SocketAddr>) -> Result<Running> {
    check_data_dir(data)?;
    let cfg = Config::load(data)?;
    let addr: SocketAddr = match listen {
        Some(a) => a,
        None => cfg.listen.parse()?,
    };
    let tls = tls::server_config(data).context("the server certificate is missing or damaged (run `sleuth-server init` or `renew-cert`)")?;
    let left = tls::expires(&data.join(tls::SERVER_CERT))? - sleuth_engine::service::now();
    if left < 0 {
        bail!("the server certificate has expired; run `sleuth-server renew-cert`");
    }
    if left < 30 * 86_400 {
        warn!("the server certificate expires in {} days; run `sleuth-server renew-cert`", left / 86_400);
    }
    let audit = audit::Audit::open(data)?;
    audit.trim()?;
    let settings = cfg.engine_settings()?;
    let engine = Engine::new_server(settings);
    {
        let e = Arc::clone(&engine);
        tokio::task::spawn_blocking(move || e.start()).await??;
    }
    info!("indexing {} folder(s) in {} project(s) for {} user(s)", cfg.all_roots().len(), cfg.projects.len(), cfg.users.len());
    let state = Arc::new(AppState::new(Arc::clone(&engine), data.to_path_buf(), cfg, audit));
    let app = api::router(Arc::clone(&state));
    let handle = axum_server::Handle::new();
    let server = axum_server::bind_rustls(addr, axum_server::tls_rustls::RustlsConfig::from_config(tls)).handle(handle.clone());
    let served = tokio::spawn(server.serve(app.into_make_service_with_connect_info::<SocketAddr>()));
    let Some(addr) = handle.listening().await else {
        engine.stop();
        let e = served.await.map_err(anyhow::Error::from).and_then(|r| r.map_err(anyhow::Error::from)).err();
        bail!("cannot listen on {addr}: {}", e.map(|e| format!("{e:#}")).unwrap_or_else(|| "unknown error".into()));
    };
    info!("Sleuth server {} listening on https://{addr}/api/v1", env!("CARGO_PKG_VERSION"));
    let watcher = tokio::spawn(watch_config(Arc::clone(&state)));
    Ok(Running { addr, state, handle, served, watcher })
}

impl Running {
    /// Stop answering (finishing requests under way) and stop the engine.
    pub async fn stop(self) {
        self.watcher.abort();
        self.handle.graceful_shutdown(Some(Duration::from_secs(10)));
        let _ = self.served.await;
        self.state.engine.stop();
        info!("Sleuth server stopped");
    }
}

/// Serve until `shutdown` completes.
pub async fn run_until(data: &Path, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
    let r = start(data, None).await?;
    shutdown.await;
    r.stop().await;
    Ok(())
}

fn modified(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Pick up changes to `server.json` made with the command line.
async fn watch_config(state: Arc<AppState>) {
    let path = config::config_path(&state.data);
    let mut seen = modified(&path);
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let now = modified(&path);
        if now == seen {
            continue;
        }
        seen = now;
        let new = match Config::load(&state.data) {
            Ok(c) => c,
            Err(e) => {
                warn!("server.json changed but is not valid; still using the previous one: {e:#}");
                continue;
            }
        };
        let old = state.config();
        if new == *old {
            continue;
        }
        let (old_s, new_s) = (old.engine_settings(), new.engine_settings());
        *state.cfg.write().unwrap() = Arc::new(new);
        info!("server.json reloaded");
        if let (Ok(a), Ok(b)) = (old_s, new_s)
            && a != b
        {
            let engine = Arc::clone(&state.engine);
            let r = tokio::task::spawn_blocking(move || {
                engine.set_settings(b)?;
                engine.rescan_now();
                anyhow::Ok(())
            })
            .await;
            match r {
                Ok(Ok(())) => info!("project folders or reader settings changed; walking the folders again"),
                Ok(Err(e)) => warn!("could not apply the new settings: {e:#}"),
                Err(e) => warn!("could not apply the new settings: {e}"),
            }
        }
    }
}

/// The data folder from `--data`, or the default.
pub fn data_dir(arg: Option<PathBuf>) -> PathBuf {
    arg.unwrap_or_else(config::default_data_dir)
}
