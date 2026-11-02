//! Walking folders on local and network drives without hanging.
//!
//! - Roots are grouped by file server (`U:` and `X:` on `\\fs02` share
//!   one group), and each group gets its own small pool of listing threads, so
//!   a slow or busy server never holds up the others and is not overloaded.
//! - Each root is probed first; a share that does not answer in time is
//!   reported offline and skipped.
//! - A network folder listing that exceeds the timeout is reported as skipped
//!   with the reason, and the walk carries on.
//! - Junctions and other reparse points are not followed (they can loop).
//! - Setting the cancel flag stops every thread within one listing.
//! - A listing that timed out leaves its helper thread waiting on Windows.
//!   At most [`MAX_STUCK`] such threads are allowed per server; beyond that
//!   the server is treated as not answering until they return, so a hung
//!   share cannot pile up threads.
//! - An interrupted walk can resume: folders already listed in the same pass
//!   are not listed again (their known subfolders are walked instead).

use sleuth_core::paths::server_key;
use std::collections::{HashMap, VecDeque};
use std::os::windows::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, UNIX_EPOCH};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const ROOT_PROBE: Duration = Duration::from_secs(4);
/// Timed-out listings allowed to be still waiting, per server.
pub const MAX_STUCK: usize = 4;

/// Listings per server that timed out and have not returned yet.
fn stuck(server: &str) -> Arc<AtomicUsize> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<String, Arc<AtomicUsize>>>> = std::sync::OnceLock::new();
    MAP.get_or_init(Default::default).lock().unwrap().entry(server.to_string()).or_default().clone()
}

pub fn stuck_count(server: &str) -> usize {
    stuck(server).load(Ordering::SeqCst)
}

#[derive(Debug, Clone)]
pub struct WalkConfig {
    pub per_server: usize,
    pub local_threads: usize,
    pub listing_timeout: Duration,
    /// Lower-case path prefixes never entered.
    pub excluded: Vec<String>,
    /// Run listing threads in Windows background mode (low CPU and I/O priority).
    pub background: bool,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug)]
pub enum WalkEvent {
    Listed { root: String, dir: String, entries: Vec<Entry> },
    Skipped { root: String, dir: String, reason: String },
    RootOffline { root: String, reason: String },
    RootDone { root: String },
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') { format!("{dir}{name}") } else { format!("{dir}\\{name}") }
}

/// Folder path used as `dir` in the index: no trailing backslash except for
/// drive roots (`C:\`).
pub fn norm_dir(p: &str) -> String {
    let t = p.trim_end_matches('\\');
    if t.len() == 2 && t.ends_with(':') { format!("{t}\\") } else { t.to_string() }
}

fn list_dir(dir: &str) -> Result<Vec<Entry>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| describe(&e))?;
    let mut out = Vec::new();
    for ent in rd {
        let Ok(ent) = ent else { continue };
        let Ok(md) = ent.metadata() else { continue };
        let attrs = md.file_attributes();
        let is_dir = md.is_dir();
        if is_dir && attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            continue;
        }
        let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
        out.push(Entry { name: ent.file_name().to_string_lossy().into_owned(), is_dir, size: if is_dir { 0 } else { md.len() }, mtime });
    }
    Ok(out)
}

fn describe(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "access denied".into(),
        std::io::ErrorKind::NotFound => "no longer exists".into(),
        _ => e.to_string(),
    }
}

/// List on a helper thread and give up after `timeout`. A stuck listing
/// thread is abandoned and counted against its server; it ends by itself
/// when Windows finally answers.
pub fn list_with_timeout(dir: &str, timeout: Duration) -> Result<Vec<Entry>, String> {
    #[derive(PartialEq)]
    enum Call {
        Running,
        TimedOut,
        Done,
    }
    let counter = stuck(&server_key(dir));
    let waiting = counter.load(Ordering::SeqCst);
    if waiting >= MAX_STUCK {
        return Err(format!("server not answering ({waiting} earlier listings still waiting)"));
    }
    let (tx, rx) = mpsc::channel();
    let d = dir.to_string();
    let call = Arc::new(Mutex::new(Call::Running));
    let (call2, counter2) = (Arc::clone(&call), Arc::clone(&counter));
    std::thread::Builder::new()
        .name("sleuth-list".into())
        .spawn(move || {
            let r = list_dir(&d);
            let mut c = call2.lock().unwrap();
            if *c == Call::TimedOut {
                counter2.fetch_sub(1, Ordering::SeqCst);
            }
            *c = Call::Done;
            drop(c);
            let _ = tx.send(r);
        })
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => {
            let mut c = call.lock().unwrap();
            if *c == Call::Done {
                // Answered just as the time ran out.
                drop(c);
                return rx.recv().unwrap_or_else(|_| Err("listing failed".into()));
            }
            *c = Call::TimedOut;
            counter.fetch_add(1, Ordering::SeqCst);
            Err(format!("no answer within {} s", timeout.as_secs()))
        }
    }
}

fn is_network(p: &str) -> bool {
    p.starts_with("\\\\")
}

struct Queue {
    items: VecDeque<(String, String)>,
    /// Folders queued or being listed.
    pending: usize,
}

/// Walk all roots; `sink` receives events from many threads. Returns when
/// every root is finished, offline or the walk was cancelled.
pub fn walk(roots: &[String], cfg: &WalkConfig, cancel: &Arc<AtomicBool>, sink: &(dyn Fn(WalkEvent) + Sync)) {
    walk_resumable(roots, cfg, cancel, &|_| None, sink)
}

/// [`walk`], skipping folders for which `done` returns their subfolders
/// (already listed earlier in the same pass).
pub fn walk_resumable(roots: &[String], cfg: &WalkConfig, cancel: &Arc<AtomicBool>, done: &(dyn Fn(&str) -> Option<Vec<String>> + Sync), sink: &(dyn Fn(WalkEvent) + Sync)) {
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for r in roots {
        groups.entry(server_key(r)).or_default().push(r.clone());
    }
    std::thread::scope(|s| {
        for (_, group) in groups {
            s.spawn(move || walk_group(&group, cfg, cancel, done, sink));
        }
    });
}

fn walk_group(roots: &[String], cfg: &WalkConfig, cancel: &Arc<AtomicBool>, done: &(dyn Fn(&str) -> Option<Vec<String>> + Sync), sink: &(dyn Fn(WalkEvent) + Sync)) {
    let network = roots.first().is_some_and(|r| is_network(r));
    let mut live = Vec::new();
    for r in roots {
        let probe = list_with_timeout(r, ROOT_PROBE);
        match probe {
            Ok(_) => live.push(r.clone()),
            Err(reason) => sink(WalkEvent::RootOffline { root: r.clone(), reason }),
        }
    }
    if live.is_empty() {
        return;
    }
    let q = Arc::new((Mutex::new(Queue { items: live.iter().map(|r| (r.clone(), r.clone())).collect(), pending: live.len() }), Condvar::new()));
    let remaining: Mutex<HashMap<String, usize>> = Mutex::new(live.iter().map(|r| (r.clone(), 1usize)).collect());
    let threads = if network { cfg.per_server.max(1) } else { cfg.local_threads.max(1) };
    std::thread::scope(|s| {
        for _ in 0..threads {
            let q = Arc::clone(&q);
            let remaining = &remaining;
            s.spawn(move || {
                if cfg.background {
                    crate::sys::enter_background_mode();
                }
                loop {
                    let next = {
                        let (m, cv) = &*q;
                        let mut g = m.lock().unwrap();
                        loop {
                            if cancel.load(Ordering::Relaxed) {
                                cv.notify_all();
                                return;
                            }
                            if let Some(item) = g.items.pop_front() {
                                break item;
                            }
                            if g.pending == 0 {
                                cv.notify_all();
                                return;
                            }
                            g = cv.wait_timeout(g, Duration::from_millis(200)).unwrap().0;
                        }
                    };
                    let (root, dir) = next;
                    let mut subdirs = Vec::new();
                    let resumed = done(&norm_dir(&dir));
                    let listed = match resumed {
                        Some(kids) => {
                            subdirs.extend(kids.into_iter().filter(|p| {
                                let lower = p.to_lowercase();
                                !cfg.excluded.iter().any(|x| lower.starts_with(x.as_str()))
                            }));
                            None
                        }
                        None if is_network(&dir) => Some(list_with_timeout(&dir, cfg.listing_timeout)),
                        None => Some(list_dir(&dir)),
                    };
                    match listed {
                        None => {}
                        Some(Ok(entries)) => {
                            for e in entries.iter().filter(|e| e.is_dir) {
                                let p = join(&dir, &e.name);
                                let lower = p.to_lowercase();
                                if cfg.excluded.iter().any(|x| lower.starts_with(x.as_str())) || skip_name(&e.name) {
                                    continue;
                                }
                                subdirs.push(p);
                            }
                            sink(WalkEvent::Listed { root: root.clone(), dir: norm_dir(&dir), entries });
                        }
                        Some(Err(reason)) => sink(WalkEvent::Skipped { root: root.clone(), dir: dir.clone(), reason }),
                    }
                    let root_finished = {
                        let mut rem = remaining.lock().unwrap();
                        let n = rem.get_mut(&root).unwrap();
                        *n += subdirs.len();
                        *n -= 1;
                        *n == 0
                    };
                    if root_finished && !cancel.load(Ordering::Relaxed) {
                        sink(WalkEvent::RootDone { root: root.clone() });
                    }
                    let (m, cv) = &*q;
                    let mut g = m.lock().unwrap();
                    g.pending += subdirs.len();
                    g.pending -= 1;
                    // Depth-first keeps the queue (and memory) small on deep trees.
                    for d in subdirs.into_iter().rev() {
                        g.items.push_front((root.clone(), d));
                    }
                    cv.notify_all();
                }
            });
        }
    });
}

fn skip_name(name: &str) -> bool {
    matches!(name, "System Volume Information" | "$Recycle.Bin" | "$RECYCLE.BIN" | "Config.Msi" | ".git" | "node_modules")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn walks_a_temp_tree_and_respects_exclusions() {
        let base = std::env::temp_dir().join(format!("dh-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a/b/c")).unwrap();
        std::fs::create_dir_all(base.join("skip/me")).unwrap();
        std::fs::write(base.join("a/b/c/PSV-1203.pdf"), b"x").unwrap();
        std::fs::write(base.join("skip/me/x.pdf"), b"x").unwrap();
        let root = base.to_string_lossy().to_string();
        let cfg = WalkConfig {
            per_server: 2,
            local_threads: 3,
            listing_timeout: Duration::from_secs(5),
            excluded: vec![base.join("skip").to_string_lossy().to_lowercase()],
            background: false,
        };
        let files = Mutex::new(Vec::new());
        let done = AtomicUsize::new(0);
        walk(&[root], &cfg, &Arc::new(AtomicBool::new(false)), &|ev| match ev {
            WalkEvent::Listed { entries, .. } => files.lock().unwrap().extend(entries.into_iter().filter(|e| !e.is_dir).map(|e| e.name)),
            WalkEvent::RootDone { .. } => {
                done.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        });
        assert_eq!(*files.lock().unwrap(), vec!["PSV-1203.pdf".to_string()]);
        assert_eq!(done.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resumed_walk_skips_listed_folders() {
        let base = std::env::temp_dir().join(format!("dh-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a").join("b")).unwrap();
        std::fs::write(base.join("a").join("b").join("x.pdf"), b"x").unwrap();
        let root = base.to_string_lossy().to_string();
        let a = base.join("a").to_string_lossy().to_string();
        let b = base.join("a").join("b").to_string_lossy().to_string();
        let cfg = WalkConfig { per_server: 1, local_threads: 2, listing_timeout: Duration::from_secs(5), excluded: vec![], background: false };
        let listed = Mutex::new(Vec::new());
        // the root and "a" were listed before the interruption
        let done = |d: &str| {
            if d.eq_ignore_ascii_case(&norm_dir(&root)) {
                Some(vec![a.clone()])
            } else if d.eq_ignore_ascii_case(&a) {
                Some(vec![b.clone()])
            } else {
                None
            }
        };
        walk_resumable(&[root.clone()], &cfg, &Arc::new(AtomicBool::new(false)), &done, &|ev| {
            if let WalkEvent::Listed { dir, .. } = ev {
                listed.lock().unwrap().push(dir);
            }
        });
        let l = listed.lock().unwrap();
        assert_eq!(*l, vec![norm_dir(&b)], "only the folder not listed before");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stuck_listings_are_capped_per_server() {
        let k = stuck(&server_key(r"\\nowhere-test\share"));
        k.store(MAX_STUCK, Ordering::SeqCst);
        let r = list_with_timeout(r"\\nowhere-test\share\x", Duration::from_secs(1));
        assert!(r.unwrap_err().contains("not answering"));
        k.store(0, Ordering::SeqCst);
    }

    #[test]
    fn missing_root_is_offline() {
        let cfg = WalkConfig { per_server: 1, local_threads: 1, listing_timeout: Duration::from_secs(2), excluded: vec![], background: false };
        let offline = AtomicUsize::new(0);
        walk(&[r"Q:\does\not\exist".into()], &cfg, &Arc::new(AtomicBool::new(false)), &|ev| {
            if let WalkEvent::RootOffline { .. } = ev {
                offline.fetch_add(1, Ordering::SeqCst);
            }
        });
        assert_eq!(offline.load(Ordering::SeqCst), 1);
    }
}
