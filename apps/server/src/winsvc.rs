//! Running as the Windows service `SleuthServer`.
//!
//! - Installed to start automatically as the virtual account
//!   `NT SERVICE\SleuthServer`: no password to manage, no rights beyond what
//!   is granted. On a domain it reaches shares as the computer account
//!   (`DOMAIN\SERVER$`), so either that account gets read access to the
//!   project shares or IT switches the service to a domain service account
//!   (see docs/SERVER.md).
//! - `install` also locks the data folder down to Administrators, SYSTEM and
//!   the service (it holds the CA key and the access-key hashes).
//! - The service runs `sleuth-server.exe service-run --data "<folder>"`.

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "SleuthServer";
pub const SERVICE_ACCOUNT: &str = r"NT SERVICE\SleuthServer";

static DATA: OnceLock<PathBuf> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Hand the process to the service control manager (blocks until stopped).
pub fn run_as_service(data: PathBuf) -> Result<()> {
    let _ = DATA.set(data);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| anyhow!("not started by the service manager: {e}"))
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = service_body() {
        tracing::error!("service stopped: {e:#}");
    }
}

fn service_body() -> Result<()> {
    let data = DATA.get().cloned().context("no data folder")?;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let tx = std::sync::Mutex::new(Some(tx));
    let status = service_control_handler::register(SERVICE_NAME, move |c| match c {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let set = |state: ServiceState, accept: ServiceControlAccept, code: u32| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(code),
            checkpoint: 0,
            wait_hint: Duration::from_secs(20),
            process_id: None,
        });
    };
    set(ServiceState::StartPending, ServiceControlAccept::empty(), 0);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let r = rt.block_on(async {
        let running = crate::start(&data, None).await?;
        set(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN, 0);
        let _ = rx.await;
        set(ServiceState::StopPending, ServiceControlAccept::empty(), 0);
        running.stop().await;
        anyhow::Ok(())
    });
    if let Err(e) = &r {
        tracing::error!("{e:#}");
    }
    set(ServiceState::Stopped, ServiceControlAccept::empty(), if r.is_ok() { 0 } else { 1 });
    r
}

fn manager(create: bool) -> Result<ServiceManager> {
    let access = if create { ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE } else { ServiceManagerAccess::CONNECT };
    ServiceManager::local_computer(None::<&str>, access).map_err(|e| anyhow!("cannot open the service manager (run as administrator): {e}"))
}

/// Allow only Administrators, SYSTEM and the service into the data folder.
///
/// The folder alone gets the permissions; everything in it then inherits
/// them. (1.2.0 ran the same command over every file with `/T`: on a file
/// it removed the inherited permissions and granted nothing, so the service
/// could not read its own server.json and stopped at once.)
fn lock_down(data: &Path) -> Result<()> {
    let run = |args: &[&std::ffi::OsStr]| -> Result<()> {
        let out = std::process::Command::new("icacls").args(args).output().context("running icacls")?;
        if !out.status.success() {
            bail!("icacls could not set the data folder's permissions: {}", String::from_utf8_lossy(&out.stdout).trim());
        }
        Ok(())
    };
    let account = format!("{SERVICE_ACCOUNT}:(OI)(CI)M");
    run(&[data.as_os_str(), "/inheritance:r".as_ref(), "/grant:r".as_ref(), "*S-1-5-32-544:(OI)(CI)F".as_ref(), "*S-1-5-18:(OI)(CI)F".as_ref(), account.as_ref(), "/Q".as_ref()])?;
    // Whatever is inside takes the folder's permissions (and only those).
    let inside = data.join("*");
    run(&[inside.as_os_str(), "/reset".as_ref(), "/T".as_ref(), "/C".as_ref(), "/Q".as_ref()])
}

/// Repair a data folder locked down by 1.2.0 (files nobody could read):
/// run on every start of the server window and of `service install`.
pub fn repair_permissions(data: &Path) -> Result<()> {
    if data.exists() { lock_down(data) } else { Ok(()) }
}

pub fn install(data: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "Sleuth office server".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec!["service-run".into(), "--data".into(), data.as_os_str().to_owned()],
        dependencies: vec![],
        account_name: Some(SERVICE_ACCOUNT.into()),
        account_password: None,
    };
    let svc = manager(true)?.create_service(&info, ServiceAccess::CHANGE_CONFIG).map_err(|e| anyhow!("cannot create the service {SERVICE_NAME}: {e}"))?;
    let _ = svc.set_description("Shared drawing index and search for Sleuth desktops (HTTPS API).");
    // The virtual account exists once the service does.
    lock_down(data)?;
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let svc = manager(false)?.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE).map_err(|e| anyhow!("no service {SERVICE_NAME}: {e}"))?;
    if svc.query_status()?.current_state != ServiceState::Stopped {
        let _ = svc.stop();
    }
    svc.delete().map_err(|e| anyhow!("cannot remove the service: {e}"))?;
    Ok(())
}

pub fn start() -> Result<()> {
    let svc = manager(false)?.open_service(SERVICE_NAME, ServiceAccess::START).map_err(|e| anyhow!("no service {SERVICE_NAME}: {e}"))?;
    svc.start::<&str>(&[]).map_err(|e| anyhow!("cannot start the service (see the log in the data folder): {e}"))?;
    Ok(())
}

/// "not installed", "running", "stopped", "starting", "stopping".
pub fn state() -> &'static str {
    let Ok(m) = manager(false) else { return "unknown" };
    let Ok(svc) = m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) else { return "not installed" };
    match svc.query_status().map(|s| s.current_state) {
        Ok(ServiceState::Running) => "running",
        Ok(ServiceState::Stopped) => "stopped",
        Ok(ServiceState::StartPending) => "starting",
        Ok(ServiceState::StopPending) => "stopping",
        Ok(_) => "paused",
        Err(_) => "unknown",
    }
}

/// Stop and wait until it has stopped (up to `secs`).
pub fn stop_and_wait(secs: u64) -> Result<()> {
    if matches!(state(), "not installed" | "stopped") {
        return Ok(());
    }
    let _ = stop();
    for _ in 0..secs * 2 {
        if matches!(state(), "stopped" | "not installed") {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!("the service did not stop within {secs} s")
}

pub fn stop() -> Result<()> {
    let svc = manager(false)?.open_service(SERVICE_NAME, ServiceAccess::STOP).map_err(|e| anyhow!("no service {SERVICE_NAME}: {e}"))?;
    svc.stop().map_err(|e| anyhow!("cannot stop the service: {e}"))?;
    Ok(())
}
