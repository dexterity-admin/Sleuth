//! `sleuth-worker.exe`: keeps the file list, has documents read and answers
//! the Sleuth window over a per-user named pipe. One instance per user.
//!
//! Started as `sleuth-worker --reader fast|ocr` it is a reader process
//! instead: it reads the files the worker hands it and nothing else.

#![windows_subsystem = "windows"]

use anyhow::Result;
use sleuth_core::protocol::{Envelope, Event, PROTOCOL_VERSION, Request, Stage, instance_name, pipe_name};
use sleuth_core::settings::{Settings, data_dir};
use sleuth_engine::service::{Engine, EventTx};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tracing::{info, warn};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::HSTRING;

fn init_logging() {
    let dir = data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("worker.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(&path, dir.join("worker.old.log"));
    }
    if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("SLEUTH_LOG").unwrap_or_else(|_| "info".into()))
            .init();
    }
}

/// Exit quietly when another worker already runs for this user.
fn single_instance() -> bool {
    let name = HSTRING::from(format!("Local\\Sleuth-worker-{}", instance_name()));
    unsafe {
        let h = CreateMutexW(None, true, &name);
        // The handle is kept for the life of the process on purpose.
        h.is_ok() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// Started as administrator for a signed-in user ("Read folders that need
/// administrator rights"): that user's SID, which alone may open the pipe.
static ELEVATED_FOR: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// `--elevated --user NAME --sid SID --appdata DIR --localappdata DIR`: run
/// as the user who asked (their settings, index and pipe), with the rights
/// of the administrator account Windows was given. Before any thread starts.
fn take_elevated_args() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("--elevated") {
        return;
    }
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
    let (user, sid) = (get("--user"), get("--sid"));
    if user.is_empty() || !sid.starts_with("S-1-") || !sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return;
    }
    // SAFETY: called first thing in main, before any other thread exists.
    unsafe {
        std::env::set_var("USERNAME", &user);
        for (k, v) in [("APPDATA", get("--appdata")), ("LOCALAPPDATA", get("--localappdata"))] {
            if !v.is_empty() {
                std::env::set_var(k, v);
            }
        }
    }
    let _ = ELEVATED_FOR.set(sid);
}

/// A pipe instance: as Windows makes it, or, when elevated, one the user
/// who asked can open (an administrator's pipe is closed to them otherwise).
fn new_pipe(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    let mut o = ServerOptions::new();
    o.first_pipe_instance(first).reject_remote_clients(true);
    let Some(sid) = ELEVATED_FOR.get() else { return o.create(name) };
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    let sddl = HSTRING::from(format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})"));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: valid SDDL string and out pointer; the descriptor is kept for
    // the life of the process (a few pipe instances' worth).
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None) }.map_err(std::io::Error::other)?;
    let mut sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
    // SAFETY: `sa` points at a valid security descriptor for this call.
    unsafe { o.create_with_security_attributes_raw(name, &mut sa as *mut _ as *mut std::ffi::c_void) }
}

fn main() -> Result<()> {
    take_elevated_args();
    if std::env::args().nth(1).as_deref() == Some("--reader") {
        // A reader process: no pipe, no index, no log file of its own.
        return sleuth_engine::readjob::serve();
    }
    if !single_instance() {
        return Ok(());
    }
    init_logging();
    std::panic::set_hook(Box::new(|p| tracing::error!("panic: {p}")));
    info!("Sleuth worker {} starting", env!("CARGO_PKG_VERSION"));
    let settings = Settings::load().unwrap_or_else(|e| {
        warn!("settings unreadable, using defaults: {e:#}");
        Settings::default()
    });
    let engine = Engine::new(settings);
    engine.start()?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(serve(engine))
}

async fn serve(engine: Arc<Engine>) -> Result<()> {
    let name = pipe_name();
    let clients = Arc::new(AtomicUsize::new(0));
    let mut server = new_pipe(&name, true)?;
    info!("listening on {name}");
    // Leave when the window has been closed for a while and background
    // reading is switched off.
    {
        let engine = Arc::clone(&engine);
        let clients = Arc::clone(&clients);
        tokio::spawn(async move {
            let mut quiet = 0u32;
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let keep = engine.settings.read().unwrap().keep_indexing_when_closed;
                quiet = if clients.load(Ordering::SeqCst) == 0 { quiet + 1 } else { 0 };
                if engine.shutdown.load(Ordering::SeqCst) || (!keep && quiet >= 2) {
                    engine.stop();
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    std::process::exit(0);
                }
            }
        });
    }
    loop {
        server.connect().await?;
        let conn = server;
        server = new_pipe(&name, false)?;
        let engine = Arc::clone(&engine);
        let clients = Arc::clone(&clients);
        clients.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            if let Err(e) = handle(conn, engine).await {
                info!("client closed: {e:#}");
            }
            clients.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

async fn handle(conn: NamedPipeServer, engine: Arc<Engine>) -> Result<()> {
    let (rd, mut wr) = tokio::io::split(conn);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Envelope<Event>>();
    let writer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let mut line = serde_json::to_vec(&ev).unwrap_or_default();
            line.push(b'\n');
            if wr.write_all(&line).await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(rd).lines();
    let mut my_searches = Vec::new();
    while let Some(line) = lines.next_line().await? {
        let req: Envelope<Request> = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Envelope { id: 0, body: Event::Error { message: format!("bad request: {e}") } });
                continue;
            }
        };
        if let Request::Search { .. } = &req.body {
            my_searches.push(req.id);
        }
        dispatch(req, &engine, &tx);
    }
    for id in my_searches {
        engine.cancel_search(id);
    }
    drop(tx);
    let _ = writer.await;
    Ok(())
}

fn reply(tx: &EventTx, id: u64, r: Result<Event>) {
    let body = r.unwrap_or_else(|e| Event::Error { message: format!("{e:#}") });
    let _ = tx.send(Envelope { id, body });
}

fn dispatch(req: Envelope<Request>, engine: &Arc<Engine>, tx: &EventTx) {
    let id = req.id;
    let e = Arc::clone(engine);
    let tx = tx.clone();
    match req.body {
        Request::Hello { .. } => reply(&tx, id, Ok(Event::Hello { protocol: PROTOCOL_VERSION, version: env!("CARGO_PKG_VERSION").into() })),
        Request::Search { query, roots, project, deep, limit, offset } => {
            tokio::task::spawn_blocking(move || {
                if let Err(err) = e.search(id, &query, &roots, project.as_deref(), deep, limit.clamp(1, 5000), offset, &tx) {
                    reply(&tx, id, Err(err));
                }
            });
        }
        Request::Cancel { search_id } => {
            e.cancel_search(search_id);
            reply(&tx, id, Ok(Event::Ok));
        }
        Request::Preview { path, page, query, max_px, tag, office } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.preview(&path, page, &query, max_px, tag.as_deref(), office)));
        }
        Request::Tags { category, contains, project, limit, offset, office } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.tags(category.as_deref(), contains.as_deref(), project.as_deref(), limit, offset, office)));
        }
        Request::TagOccurrences { norm, project, office } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.occurrences(&norm, project.as_deref(), office)));
        }
        Request::Revisions { contains, project, limit, office } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.revisions(contains.as_deref(), project.as_deref(), limit, office)));
        }
        Request::Compare { old, new, office } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.compare(&old, &new, office)));
        }
        Request::Related { path, office } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.related(&path, office)));
        }
        Request::DocInfo { path, office } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.doc_info(&path, office)));
        }
        Request::LowConfidence { path, limit, offset } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.low_confidence(path.as_deref(), limit, offset)));
        }
        Request::Crop { path, page, bbox, max_px } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.crop(&path, page, bbox, max_px)));
        }
        Request::Correct { line_id, text } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.correct(line_id, &text)));
        }
        Request::Uncorrect { correction_id } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.uncorrect(correction_id)));
        }
        Request::Monitor => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.monitor().map(|m| Event::Monitor { monitor: Box::new(m) })));
        }
        Request::History => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.history()));
        }
        Request::Pin { query, pinned } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.pin(&query, pinned)));
        }
        Request::CheckServer { url, token, ca_file } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, Ok(Event::Server { server: e.check_server(&url, &token, &ca_file) })));
        }
        Request::SetServer { url, token, ca_file } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.set_server(&url, &token, &ca_file).map(|_| Event::Ok)));
        }
        Request::JoinServer => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.join_server().map(|server| Event::Server { server })));
        }
        Request::Status => {
            tokio::task::spawn_blocking(move || {
                reply(
                    &tx,
                    id,
                    e.status().map(|(mut status, failed)| {
                        status.elevated = ELEVATED_FOR.get().is_some();
                        Event::Status { status, failed }
                    }),
                )
            });
        }
        Request::Drives => {
            tokio::task::spawn_blocking(move || {
                let drives = e.current_drives();
                *e.drives.write().unwrap() = drives.clone();
                reply(&tx, id, Ok(Event::Drives { drives: drives.drives }));
            });
        }
        Request::GetSettings => reply(&tx, id, Ok(Event::Settings { settings: Box::new(e.settings.read().unwrap().clone()) })),
        Request::SetSettings { settings } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.set_settings(*settings).map(|_| Event::Ok)));
        }
        Request::RescanNow => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.run_now(Stage::Walk, None).map(|_| Event::Ok)));
        }
        Request::SetPaused { stage, paused } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.set_paused(stage, paused).map(|_| Event::Ok)));
        }
        Request::RunNow { stage, root } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.run_now(stage, root.as_deref()).map(|_| Event::Ok)));
        }
        Request::SetSpeed { mode, minutes } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.set_speed(mode, minutes).map(|_| Event::Ok)));
        }
        Request::TextPreview { path, page, query, tag } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.text_preview(&path, page, &query, tag.as_deref())));
        }
        Request::ReadNow { path } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.read_now(&path).map(|_| Event::Ok)));
        }
        Request::CheckCache { dir } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, Ok(Event::Cache { check: e.check_cache_dir(&dir) })));
        }
        Request::Shutdown => {
            reply(&tx, id, Ok(Event::Ok));
            e.stop();
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                std::process::exit(0);
            });
        }
    }
}
