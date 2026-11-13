//! The Sleuth Server window: what double-clicking `sleuth-server.exe` opens.
//!
//! One window (WebView2, as the desktop app) with the whole job in it:
//! first-time setup in one form, then Overview (service, address, what the
//! server reads), People and PCs (keys, PCs that connected themselves),
//! Projects, Certificate and Log. The command line (`sleuth-server init`,
//! `add-user`, ...) stays for scripts.
//!
//! The page asks for work with `window.ipc.postMessage({id, cmd, args})`;
//! each command runs on its own thread and its answer comes back through
//! the event loop as `window.__reply(id, ok, value)`.
//!
//! The window needs administrator rights (the data folder is locked to
//! Administrators, the service is installed and started from it): it asks
//! Windows for them when opened without.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use sleuth_server::audit::{Audit, cli_user};
use sleuth_server::config::{self, Config, Role};
use sleuth_server::{card, tls, winsvc};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

const PAGE: &str = include_str!("gui.html");

enum UserEvent {
    Reply(String),
}

fn is_elevated() -> bool {
    std::process::Command::new("whoami").arg("/groups").output().map(|o| String::from_utf8_lossy(&o.stdout).contains("S-1-16-12288")).unwrap_or(false)
}

/// Start `exe gui` as administrator (Windows asks). True when it started.
fn relaunch_elevated(exe: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    let cmd = format!("Start-Process -FilePath '{}' -ArgumentList 'gui' -Verb RunAs", exe.display().to_string().replace('\'', "''"));
    std::process::Command::new("powershell").args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &cmd]).creation_flags(0x0800_0000).status().is_ok_and(|s| s.success())
}

/// Opened from Explorer, the console window belongs to this program alone:
/// let it go, the window is what the person uses.
fn hide_console() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut list = [0u32; 4];
    // SAFETY: the buffer is valid for its length.
    let n = unsafe { GetConsoleProcessList(&mut list) };
    if n <= 1 {
        // SAFETY: no handles of the console are kept.
        let _ = unsafe { FreeConsole() };
    }
}

fn program_home() -> PathBuf {
    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into())).join("Sleuth Server")
}

/// The service runs the program from a fixed folder. Opened from anywhere
/// else (Downloads, a new version's zip folder), it is copied there,
/// stopping the service while its files are replaced. Returns the copy.
fn install_home(exe: &Path) -> Result<Option<PathBuf>> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    let home = program_home();
    if dir.display().to_string().to_lowercase().starts_with(&home.display().to_string().to_lowercase()) {
        return Ok(None);
    }
    let temp = std::env::var("TEMP").unwrap_or_default().to_lowercase();
    if (!temp.is_empty() && dir.display().to_string().to_lowercase().starts_with(&temp)) || !dir.join("pdfium.dll").exists() {
        bail!("This was opened straight from the zip file. Close this window, right-click the zip > Extract All..., then open sleuth-server.exe from the extracted folder.");
    }
    let was_running = winsvc::state() == "running";
    winsvc::stop_and_wait(60).context("stopping the service to update its files")?;
    copy_dir(dir, &home)?;
    if was_running {
        let _ = winsvc::start();
    }
    Ok(Some(home.join("sleuth-server.exe")))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("cannot create {}", to.display()))?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), &dst).with_context(|| format!("cannot copy {} (is a Sleuth Server window still open from there?)", e.path().display()))?;
        }
    }
    Ok(())
}

/// Open the window. Returns when it is closed.
pub fn run(data: PathBuf) -> Result<()> {
    hide_console();
    let exe = std::env::current_exe()?;
    if !is_elevated() {
        if relaunch_elevated(&exe) {
            return Ok(());
        }
        return message_box("Sleuth Server needs administrator rights to set up and run the server. Right-click sleuth-server.exe > Run as administrator.");
    }
    match install_home(&exe) {
        Ok(Some(new_exe)) => {
            std::process::Command::new(&new_exe).arg("gui").spawn().with_context(|| format!("cannot start {}", new_exe.display()))?;
            return Ok(());
        }
        Ok(None) => {}
        Err(e) => return message_box(&format!("{e:#}")),
    }
    sleuth_server::use_data_dir(&data);
    // 1.2.0 left files in the data folder that nobody, the service included,
    // could read; put the permissions right and start a service that stopped for it.
    if winsvc::repair_permissions(&data).is_ok() && winsvc::state() == "stopped" && config::config_path(&data).exists() {
        let _ = winsvc::start();
    }
    window(data)
}

fn message_box(text: &str) -> Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    // SAFETY: plain strings, no owner window.
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Sleuth Server"), MB_OK | MB_ICONWARNING) };
    Ok(())
}

fn window(data: PathBuf) -> Result<()> {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title(format!("Sleuth Server {}", env!("CARGO_PKG_VERSION")))
        .with_inner_size(tao::dpi::LogicalSize::new(1100.0, 760.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(760.0, 520.0))
        .build(&event_loop)?;
    let profile = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Sleuth Server").join("WebView2");
    let mut ctx = wry::WebContext::new(Some(profile));
    let d = data.clone();
    let webview = WebViewBuilder::new_with_web_context(&mut ctx)
        .with_html(PAGE)
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            let Ok(msg) = serde_json::from_str::<Value>(req.body()) else { return };
            let id = msg["id"].as_u64().unwrap_or(0);
            let cmd = msg["cmd"].as_str().unwrap_or("").to_string();
            let args = msg["args"].clone();
            let (data, proxy) = (d.clone(), proxy.clone());
            std::thread::spawn(move || {
                let r = command(&data, &cmd, &args);
                let (ok, v) = match r {
                    Ok(v) => (true, v),
                    Err(e) => (false, Value::String(format!("{e:#}"))),
                };
                let _ = proxy.send_event(UserEvent::Reply(format!("window.__reply({id},{ok},{v})")));
            });
        })
        .build(&window)
        .map_err(|e| anyhow!("cannot open the window (is the Microsoft Edge WebView2 runtime installed?): {e}"))?;
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Reply(js)) => {
                let _ = webview.evaluate_script(&js);
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *flow = ControlFlow::Exit,
            _ => {}
        }
    });
}

// ---- commands ----------------------------------------------------------------

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").trim().to_string()
}

fn list(v: &Value, k: &str) -> Vec<String> {
    v[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()).unwrap_or_default()
}

fn to_unc(p: &str) -> String {
    sleuth_core::paths::DriveMap::current().to_unc(p.trim())
}

fn audit(data: &Path, action: &str, target: &str, ok: bool) {
    if let Ok(a) = Audit::open(data) {
        a.record(&cli_user(), action, target, if ok { "ok" } else { "failed" });
    }
}

/// Change `server.json`, record it, and refresh the card (join settings,
/// certificate) in the office index folder.
fn change(data: &Path, action: &str, target: &str, f: impl FnOnce(&mut Config) -> Result<()>) -> Result<()> {
    let mut c = Config::load(data)?;
    let r = f(&mut c).and_then(|_| c.save(data));
    audit(data, action, target, r.is_ok());
    r?;
    let _ = publish(data);
    Ok(())
}

fn publish(data: &Path) -> Result<Value> {
    let c = card::ensure_join_code(data)?;
    match card::publish(data, &c)? {
        Some(p) => Ok(json!({ "path": p.display().to_string() })),
        None => bail!("no office index folder is set"),
    }
}

fn command(data: &Path, cmd: &str, a: &Value) -> Result<Value> {
    match cmd {
        "state" => state(data),
        "check_folder" => {
            let p = to_unc(&s(a, "path"));
            if p.is_empty() {
                bail!("type a folder");
            }
            match sleuth_engine::crawl::list_with_timeout(&p, Duration::from_secs(10)) {
                Ok(e) => Ok(json!({ "path": p, "entries": e.len() })),
                Err(e) => bail!("{p}: {e}"),
            }
        }
        "setup" => setup(data, a),
        "add_person" => {
            let name = s(a, "name");
            if name.is_empty() {
                bail!("type the person's name");
            }
            let role = if a["admin"].as_bool().unwrap_or(false) { Role::Admin } else { Role::User };
            let projects = { let p = list(a, "projects"); if p.is_empty() { vec!["*".to_string()] } else { p } };
            let key = crate::create_user(data, &name, role, &projects)?;
            Ok(json!({ "name": name, "key": key }))
        }
        "remove_person" => {
            let name = s(a, "name");
            change(data, "remove user", &name, |c| {
                let before = c.users.len();
                c.users.retain(|u| !u.name.eq_ignore_ascii_case(&name));
                if c.users.len() == before {
                    bail!("no one called {name}");
                }
                Ok(())
            })?;
            Ok(Value::Null)
        }
        "set_join" => {
            let on = a["enabled"].as_bool().unwrap_or(true);
            let projects = { let p = list(a, "projects"); if p.is_empty() { vec!["*".to_string()] } else { p } };
            change(data, "self-registration", if on { "on" } else { "off" }, |c| {
                c.self_registration = on;
                c.join_projects = projects;
                Ok(())
            })?;
            Ok(Value::Null)
        }
        "new_join_code" => {
            change(data, "new join code", "", |c| {
                c.join_code = config::new_join_code()?;
                Ok(())
            })?;
            Ok(Value::Null)
        }
        "add_project" => {
            let name = s(a, "name");
            let roots: Vec<String> = list(a, "folders").iter().map(|f| to_unc(f)).collect();
            let p = config::parse_project(&format!("{name}={}", roots.join(",")))?;
            change(data, "add project", &name, |c| {
                if c.project(&p.name).is_some() {
                    bail!("there is already a project called {}", p.name);
                }
                c.projects.push(p);
                Ok(())
            })?;
            Ok(Value::Null)
        }
        "remove_project" => {
            let name = s(a, "name");
            change(data, "remove project", &name, |c| {
                if c.users.iter().any(|u| u.projects.iter().any(|p| p.eq_ignore_ascii_case(&name))) {
                    bail!("someone may still search {name}; take it off their projects first");
                }
                c.join_projects.retain(|p| !p.eq_ignore_ascii_case(&name));
                if c.join_projects.is_empty() {
                    c.join_projects = vec!["*".into()];
                }
                let before = c.projects.len();
                c.projects.retain(|p| !p.name.eq_ignore_ascii_case(&name));
                if c.projects.len() == before {
                    bail!("no project called {name}");
                }
                Ok(())
            })?;
            Ok(Value::Null)
        }
        "publish" => publish(data),
        "service" => {
            let action = s(a, "action");
            let r = match action.as_str() {
                "start" => winsvc::start(),
                "stop" => winsvc::stop_and_wait(60),
                "restart" => winsvc::stop_and_wait(60).and_then(|_| winsvc::start()),
                "install" => winsvc::install(data).and_then(|_| winsvc::start()),
                _ => Err(anyhow!("unknown action {action}")),
            };
            audit(data, &format!("{action} service"), winsvc::SERVICE_NAME, r.is_ok());
            r?;
            Ok(json!({ "listening": action == "stop" || wait_listening(data) }))
        }
        "renew_cert" => {
            let c = Config::load(data)?;
            tls::issue_server_cert(data, &c.hostnames)?;
            audit(data, "renew certificate", "", true);
            let _ = winsvc::stop_and_wait(60);
            winsvc::start()?;
            Ok(Value::Null)
        }
        "ca_pem" => Ok(Value::String(std::fs::read_to_string(data.join(tls::CA_CERT)).context("reading the CA certificate")?)),
        "log" => {
            let text = std::fs::read_to_string(data.join("logs").join("server.log")).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            Ok(Value::String(lines[lines.len().saturating_sub(400)..].join("\n")))
        }
        _ => bail!("unknown command {cmd}"),
    }
}

/// Wait for the HTTPS port to answer, so "running" is checked, not assumed.
fn wait_listening(data: &Path) -> bool {
    let port = Config::load(data).ok().and_then(|c| c.listen.parse::<std::net::SocketAddr>().ok()).map(|a| a.port()).unwrap_or(8455);
    for _ in 0..20 {
        if std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(1)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

fn listening_now(c: &Config) -> bool {
    let port = c.listen.parse::<std::net::SocketAddr>().map(|a| a.port()).unwrap_or(8455);
    std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(600)).is_ok()
}

/// Everything the window shows.
fn state(data: &Path) -> Result<Value> {
    let exists = config::config_path(data).exists();
    let service = winsvc::state();
    let mut v = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "pc": tls::computer_name(),
        "data": data.display().to_string(),
        "configured": exists,
        "service": service,
        "default_folder": sleuth_core::settings::OFFICE_CACHE_UNC,
    });
    if !exists {
        return Ok(v);
    }
    let c = Config::load(data)?;
    let port = c.listen.parse::<std::net::SocketAddr>().map(|a| a.port()).unwrap_or(8455);
    v["listening"] = json!(service == "running" && listening_now(&c));
    v["address"] = json!(format!("https://{}:{port}", tls::computer_name().to_lowercase()));
    v["addresses"] = json!(tls::local_ipv4().iter().map(|ip| format!("https://{ip}:{port}")).collect::<Vec<_>>());
    v["folder"] = json!(c.shared_cache_dir);
    v["self_registration"] = json!(c.self_registration);
    v["join_projects"] = json!(c.join_projects);
    v["projects"] = json!(c.projects.iter().map(|p| json!({ "name": p.name, "folders": p.roots })).collect::<Vec<_>>());
    v["people"] = json!(c.users.iter().map(|u| json!({ "name": u.name, "role": u.role.as_str(), "projects": u.projects, "pc": u.pc, "since": u.since })).collect::<Vec<_>>());
    if let Ok(pem) = std::fs::read_to_string(data.join(tls::CA_CERT)) {
        v["ca_sha256"] = json!(tls::fingerprint(&pem).unwrap_or_default());
    }
    v["cert_expires"] = json!(tls::expires(&data.join(tls::SERVER_CERT)).unwrap_or(0));
    // The card in the office index folder: there, current, and with the
    // join code (the service may not have been allowed to write it).
    let published = card::published(&c);
    v["card"] = match &published {
        Some(p) => json!({
            "found": true,
            "at": p.at,
            "current": p.join_code == (if c.self_registration { c.join_code.clone() } else { String::new() }) && !p.ca_pem.is_empty(),
        }),
        None => json!({ "found": false }),
    };
    // What the server's engine reports to the office (its heartbeat).
    if let Some(dir) = &c.shared_cache_dir {
        let hb = Path::new(dir).join("pcs").join(format!("{}.json", sleuth_core::paths::root_key(&tls::computer_name())));
        if let Some(pc) = std::fs::read(hb).ok().and_then(|b| serde_json::from_slice::<sleuth_core::protocol::PcStatus>(&b).ok()) {
            v["engine"] = json!({ "state": pc.state, "docs_per_hour": pc.docs_per_hour, "pages_per_hour": pc.pages_per_hour, "last_seen": pc.last_seen, "readers": pc.fast_workers + pc.ocr_workers });
        }
    }
    v["now"] = json!(sleuth_engine::service::now());
    Ok(v)
}

/// First-time setup from the one form: office index folder, projects,
/// certificates, the card, the service, and a check that it answers.
fn setup(data: &Path, a: &Value) -> Result<Value> {
    if config::config_path(data).exists() {
        bail!("the server is already set up");
    }
    let mut steps: Vec<Value> = Vec::new();
    let folder = to_unc(&s(a, "folder"));
    if folder.is_empty() {
        bail!("choose the office index folder");
    }
    std::fs::create_dir_all(&folder).and_then(|_| std::fs::write(Path::new(&folder).join(".server-write-test"), b"ok")).map_err(|e| anyhow!("{folder} cannot be written: {e}"))?;
    let _ = std::fs::remove_file(Path::new(&folder).join(".server-write-test"));
    steps.push(json!(format!("Office index folder {folder} can be written.")));
    let mut projects = Vec::new();
    for p in a["projects"].as_array().cloned().unwrap_or_default() {
        let name = s(&p, "name");
        let folders: Vec<String> = list(&p, "folders").iter().map(|f| to_unc(f)).collect();
        if name.is_empty() && folders.is_empty() {
            continue;
        }
        projects.push(format!("{name}={}", folders.join(",")));
    }
    if projects.is_empty() {
        bail!("add at least one project with its folders");
    }
    let hostnames: Vec<String> = tls::local_ipv4().iter().map(|i| i.to_string()).collect();
    crate::init(data, &projects, config::DEFAULT_LISTEN.to_string(), hostnames, Some(folder.clone()))?;
    steps.push(json!(format!("{} project(s) saved; certificates made.", projects.len())));
    match publish(data) {
        Ok(_) => steps.push(json!("Address, certificate and join code left in the office index folder: PCs connect with one click.")),
        Err(e) => steps.push(json!(format!("WARNING: could not write to the office index folder: {e:#}"))),
    }
    let r = winsvc::install(data).and_then(|_| winsvc::start());
    audit(data, "setup service", winsvc::SERVICE_NAME, r.is_ok());
    match r {
        Ok(()) => steps.push(json!("Windows service installed and started (starts with Windows).")),
        Err(e) => steps.push(json!(format!("WARNING: the service: {e:#}"))),
    }
    let ok = wait_listening(data);
    steps.push(json!(if ok { "The server answers." } else { "WARNING: nothing answers yet; see Log." }));
    Ok(json!({ "steps": steps, "listening": ok }))
}
