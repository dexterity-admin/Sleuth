//! File operations as a network share needs them.

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub(crate) fn this_pc() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "this-pc".into())
}

/// `text` as part of a file name.
pub(crate) fn safe_name(text: &str) -> String {
    let s: String = text.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if s.is_empty() { "_".into() } else { s }
}

/// Windows refuses an operation on a name another process is this instant
/// deleting or replacing ("access denied"); it passes within milliseconds.
pub(crate) fn retry<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut wait = 2;
    loop {
        match op() {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && wait <= 64 => {
                std::thread::sleep(Duration::from_millis(wait));
                wait *= 2;
            }
            other => return other,
        }
    }
}

/// Write a whole file and make sure it is on disk before it gets its name.
pub(crate) fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Put `bytes` at `dest` in one step: written under `tmp`, then renamed.
pub(crate) fn replace(tmp: &Path, dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let done = write_synced(tmp, bytes).and_then(|_| retry(|| std::fs::rename(tmp, dest)));
    if done.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    done
}
