//! `sleuth-worker.exe`: indexes drives, reads PDFs and answers the Sleuth
//! window over a per-user named pipe. One instance per user.

#![windows_subsystem = "windows"]

use anyhow::Result;
use sleuth_core::protocol::{Envelope, Event, PROTOCOL_VERSION, Request, pipe_name};
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
    let user = std::env::var("USERNAME").unwrap_or_default();
    let name = HSTRING::from(format!("Local\\Sleuth-worker-{user}"));
    unsafe {
        let h = CreateMutexW(None, true, &name);
        // The handle is kept for the life of the process on purpose.
        h.is_ok() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

fn main() -> Result<()> {
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
    let mut server = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&name)?;
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
        server = ServerOptions::new().reject_remote_clients(true).create(&name)?;
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
        Request::Search { query, roots, deep, limit } => {
            tokio::task::spawn_blocking(move || {
                if let Err(err) = e.search(id, &query, &roots, deep, limit, &tx) {
                    reply(&tx, id, Err(err));
                }
            });
        }
        Request::Cancel { search_id } => {
            e.cancel_search(search_id);
            reply(&tx, id, Ok(Event::Ok));
        }
        Request::Preview { path, page, query, max_px } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.preview(&path, page, &query, max_px)));
        }
        Request::Status => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.status().map(|(status, failed)| Event::Status { status, failed })));
        }
        Request::Drives => {
            let drives = sleuth_core::paths::DriveMap::current();
            *e.drives.write().unwrap() = drives.clone();
            reply(&tx, id, Ok(Event::Drives { drives: drives.drives }));
        }
        Request::GetSettings => reply(&tx, id, Ok(Event::Settings { settings: Box::new(e.settings.read().unwrap().clone()) })),
        Request::SetSettings { settings } => {
            tokio::task::spawn_blocking(move || reply(&tx, id, e.set_settings(*settings).map(|_| Event::Ok)));
        }
        Request::RescanNow => {
            e.rescan_now();
            reply(&tx, id, Ok(Event::Ok));
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
