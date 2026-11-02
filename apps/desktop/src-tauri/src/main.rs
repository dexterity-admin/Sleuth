//! Sleuth window. A thin shell: all searching and reading happens in
//! `sleuth-worker.exe`, reached over a per-user named pipe. If the worker is
//! not running it is started; if it stops, the window reconnects.

#![windows_subsystem = "windows"]

mod update;

use sleuth_core::protocol::pipe_name;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::{mpsc, oneshot};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Sleuth reader";

#[derive(Default)]
struct Worker {
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next: AtomicU64,
}

impl Worker {
    fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn send(&self, id: u64, mut req: Value) -> Result<(), String> {
        req["id"] = json!(id);
        let line = format!("{req}\n");
        match self.tx.lock().unwrap().as_ref() {
            Some(tx) => tx.send(line).map_err(|_| "Sleuth reader is restarting, try again".to_string()),
            None => Err("Sleuth reader is starting, try again in a moment".into()),
        }
    }
}

fn worker_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.with_file_name("sleuth-worker.exe")
}

fn start_worker() {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let _ = std::process::Command::new(worker_exe()).creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).spawn();
}

/// Keep a connection to the worker open for the life of the window.
async fn connection_loop(app: AppHandle) {
    let mut started_worker = false;
    loop {
        let client = match ClientOptions::new().open(pipe_name()) {
            Ok(c) => c,
            Err(_) => {
                if !started_worker {
                    start_worker();
                    started_worker = true;
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
                continue;
            }
        };
        started_worker = false;
        let (rd, mut wr) = tokio::io::split(client);
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let worker = app.state::<Worker>();
        *worker.tx.lock().unwrap() = Some(tx);
        let _ = app.emit("worker-up", ());
        let writer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if wr.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let id = v["id"].as_u64().unwrap_or(0);
            let waiter = worker.pending.lock().unwrap().remove(&id);
            match waiter {
                Some(w) => {
                    let _ = w.send(v);
                }
                None => {
                    let _ = app.emit("worker-event", v);
                }
            }
        }
        *worker.tx.lock().unwrap() = None;
        writer.abort();
        worker.pending.lock().unwrap().clear();
        let _ = app.emit("worker-down", ());
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Send a request and wait for its single reply.
#[tauri::command]
async fn call(worker: State<'_, Worker>, req: Value) -> Result<Value, String> {
    let id = worker.next_id();
    let (tx, rx) = oneshot::channel();
    worker.pending.lock().unwrap().insert(id, tx);
    if let Err(e) = worker.send(id, req) {
        worker.pending.lock().unwrap().remove(&id);
        return Err(e);
    }
    match tokio::time::timeout(Duration::from_secs(180), rx).await {
        Ok(Ok(v)) if v["ev"] == "error" => Err(v["message"].as_str().unwrap_or("error").to_string()),
        Ok(Ok(v)) => Ok(v),
        Ok(Err(_)) => Err("Sleuth reader restarted; try again".into()),
        Err(_) => {
            worker.pending.lock().unwrap().remove(&id);
            Err("no answer from Sleuth reader".into())
        }
    }
}

/// Start a streaming search; results arrive as `worker-event` with this id.
#[tauri::command]
fn start_search(worker: State<'_, Worker>, query: String, roots: Vec<String>, deep: bool) -> Result<u64, String> {
    let id = worker.next_id();
    worker.send(id, json!({"req": "search", "query": query, "roots": roots, "deep": deep, "limit": 5000}))?;
    Ok(id)
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR};
    let r = unsafe { ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from(path.as_str()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    if r.0 as isize <= 32 { Err(format!("Windows could not open {path}")) } else { Ok(()) }
}

#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
fn pick_folder(app: AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    app.dialog().file().blocking_pick_folder().and_then(|p| p.into_path().ok()).map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<(), String> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY).map_err(|e| e.to_string())?;
    if enabled {
        key.set_value(RUN_VALUE, &format!("\"{}\"", worker_exe().display())).map_err(|e| e.to_string())
    } else {
        match key.delete_value(RUN_VALUE) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[tauri::command]
fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
async fn check_update(app: AppHandle, channel: String) -> Result<Option<update::Manifest>, String> {
    let current = app.package_info().version.to_string();
    tauri::async_runtime::spawn_blocking(move || update::check(&channel, &current)).await.map_err(|e| e.to_string())?
}

/// Download and verify the update, stop the background reader, run the
/// installer and close this window; the installer starts Sleuth again.
#[tauri::command]
async fn install_update(app: AppHandle, worker: State<'_, Worker>, channel: String) -> Result<(), String> {
    let current = app.package_info().version.to_string();
    let progress = app.clone();
    let file = tauri::async_runtime::spawn_blocking(move || {
        let Some(m) = update::check(&channel, &current)? else { return Ok(None) };
        update::download(&m, &|done, total| {
            let _ = progress.emit("update-progress", json!({"done": done, "total": total}));
        })
        .map(Some)
    })
    .await
    .map_err(|e| e.to_string())??;
    let Some(file) = file else { return Ok(()) };
    // Let the worker finish its current write and release its files.
    let id = worker.next_id();
    let _ = worker.send(id, json!({"req": "shutdown"}));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    update::run_installer(&file)?;
    app.exit(0);
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(Worker::default())
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(connection_loop(handle));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![call, start_search, open_path, reveal_path, pick_folder, set_autostart, app_version, check_update, install_update])
        .run(tauri::generate_context!())
        .expect("Sleuth failed to start");
}
