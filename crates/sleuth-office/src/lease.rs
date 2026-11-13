//! Leases on a network share: one PC at a time walks a share, compacts the
//! store, or reads a document.
//!
//! A lease is a small file, `<dir>\<name>.lease`, that only one PC can
//! create. It names its holder and runs out unless renewed, so a PC that
//! was switched off does not block the others for ever.
//!
//! - Expiry is judged on the file server's clock: the lease file's modified
//!   time plus its term, against the modified time of a file this PC has
//!   just written to the same folder. PC clocks may differ by minutes.
//! - A holder never renews once its own term has run out, and a taker waits
//!   a little longer than the term, so the two cannot meet.
//! - Whoever removes a lease (to take over an expired one, or to release
//!   its own) first renames it aside, which only one PC can do, and then
//!   looks at what it got: if that is not the file it had judged, it puts
//!   it back.

use crate::fsx::{replace, retry, safe_name};
use serde::{Deserialize, Serialize};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a measured difference to the server's clock is trusted.
const CLOCK_FOR: Duration = Duration::from_secs(600);
/// A lease file that cannot be read is being written this instant, or is
/// damage; this long after its last change it is damage.
const UNREADABLE_MS: i64 = 30_000;
/// [`Leases::sweep`] removes what ran out this long ago.
const SWEEP_MS: i64 = 600_000;
/// Sub-folders a lease name may go down.
const MAX_DEPTH: usize = 3;

/// A taker waits this much longer than the term: the clock comparison and
/// a renewal already on its way both take a moment.
fn grace_ms(ttl_s: u64) -> i64 {
    (ttl_s as i64).saturating_mul(100).max(2000)
}

fn ms_of(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

fn local_ms() -> i64 {
    ms_of(SystemTime::now())
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Random: the process-wide hash keys come from the operating system.
fn new_token() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let a = RandomState::new().hash_one((local_ms(), std::process::id(), n));
    let b = RandomState::new().hash_one((a, n));
    format!("{a:016x}{b:016x}")
}

/// Who a lease belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub host: String,
    pub user: String,
    pub pid: u32,
}

impl Identity {
    pub fn current() -> Identity {
        Identity { host: crate::fsx::this_pc(), user: std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default(), pid: std::process::id() }
    }
}

/// The holder of a lease someone else has. `until` is when it runs out
/// unless renewed, in seconds on this PC's clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub host: String,
    pub user: String,
    pub pid: u32,
    pub until: i64,
}

#[derive(Debug)]
pub enum Acquire {
    Mine(Lease),
    HeldBy(Holder),
    /// The folder could not be used (the share is away, the name is not a
    /// lease name): the caller decides whether to work without.
    Unavailable(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseFile {
    host: String,
    #[serde(default)]
    user: String,
    pid: u32,
    token: String,
    /// When it runs out, seconds on the server's clock (for people; the
    /// file's modified time plus `ttl` is what counts).
    until: i64,
    /// The term in seconds.
    #[serde(default)]
    ttl: u64,
    /// Renewals so far, so every renewal changes the file's content.
    #[serde(default)]
    n: u64,
}

impl LeaseFile {
    /// When the term ends, milliseconds on the server's clock.
    fn expires(&self, mtime_ms: i64) -> i64 {
        if self.ttl > 0 { mtime_ms.saturating_add((self.ttl as i64).saturating_mul(1000)) } else { self.until.saturating_mul(1000) }
    }
}

/// A lease file as found.
struct Found {
    raw: Vec<u8>,
    /// The server's time of its last write.
    mtime_ms: i64,
    lease: Option<LeaseFile>,
}

fn read_lease(path: &Path) -> io::Result<Found> {
    retry(|| {
        let mut f = std::fs::File::open(path)?;
        let mut raw = Vec::new();
        f.read_to_end(&mut raw)?;
        // From the open file, not the path: a path's times may come from the client's cache.
        let mtime_ms = ms_of(f.metadata()?.modified()?);
        let lease = serde_json::from_slice(&raw).ok();
        Ok(Found { raw, mtime_ms, lease })
    })
}

/// A file next to the lease: `<name>.<tag>.<ext>`.
fn beside(path: &Path, tag: &str, ext: &str) -> PathBuf {
    let leaf = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{leaf}.{tag}.{ext}"))
}

/// Remove the lease at `path` if it still is the file `found`. Renaming it
/// aside is the step only one PC can win; what was renamed is then checked,
/// and a lease that got there in between is put back.
fn take_away(path: &Path, found: &Found) -> bool {
    let tag = found.lease.as_ref().map(|l| safe_name(&l.token)).filter(|t| t.len() >= 8).unwrap_or_else(new_token);
    let dead = beside(path, &tag, "dead");
    if std::fs::rename(path, &dead).is_err() {
        return false;
    }
    match read_lease(&dead) {
        // The time too: a file another PC has created but not yet written is as empty as a damaged one.
        Ok(got) if got.raw == found.raw && got.mtime_ms == found.mtime_ms => {
            let _ = std::fs::remove_file(&dead);
            true
        }
        _ => {
            let _ = retry(|| std::fs::rename(&dead, path));
            false
        }
    }
}

/// Whether a process of this PC is still running.
#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    // SAFETY: plain calls; the handle is ours and closed before returning.
    unsafe {
        match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(h) => {
                let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
                let _ = CloseHandle(h);
                alive
            }
            // It exists but belongs to someone we may not look at.
            Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
        }
    }
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// The leases of one folder, taken as `me`.
pub struct Leases {
    dir: PathBuf,
    me: Identity,
    /// When the server's clock was last compared, and server minus local in ms.
    clock: Mutex<Option<(Instant, i64)>>,
}

impl Leases {
    pub fn new(dir: impl Into<PathBuf>, me: Identity) -> Leases {
        Leases { dir: dir.into(), me, clock: Mutex::new(None) }
    }

    /// The lease file of `name`. A name may put the file in sub-folders
    /// (`doc\3f\9a1c`); anything that could leave the folder or is not a
    /// file name is refused.
    fn file_of(&self, name: &str) -> Option<PathBuf> {
        let parts: Vec<&str> = name.split(['\\', '/']).collect();
        let good = |p: &&str| !p.is_empty() && !p.contains("..") && *p != "." && !p.ends_with(['.', ' ']) && !p.starts_with(' ') && !p.chars().any(|c| c.is_control() || "<>:\"|?*".contains(c));
        if parts.len() > MAX_DEPTH + 1 || !parts.iter().all(good) {
            return None;
        }
        let mut path = self.dir.clone();
        path.extend(&parts[..parts.len() - 1]);
        path.push(format!("{}.lease", parts[parts.len() - 1]));
        Some(path)
    }

    /// The server's clock minus this PC's, in ms: the modified time the
    /// server gives a file written now. Measured at most every ten minutes.
    fn offset(&self) -> io::Result<i64> {
        let mut g = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, off)) = *g
            && at.elapsed() < CLOCK_FOR
        {
            return Ok(off);
        }
        let path = self.dir.join(format!("clock-{}-{}-{}.probe", safe_name(&self.me.host), std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let write = || -> io::Result<i64> {
            let before = local_ms();
            let mut f = std::fs::File::create(&path)?;
            f.write_all(b"clock")?;
            f.sync_all()?;
            drop(f);
            let server = ms_of(std::fs::File::open(&path)?.metadata()?.modified()?);
            Ok(server - (before + local_ms()) / 2)
        };
        let off = match write() {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                std::fs::create_dir_all(&self.dir)?;
                write()
            }
            other => other,
        };
        let _ = std::fs::remove_file(&path);
        let off = off?;
        *g = Some((Instant::now(), off));
        Ok(off)
    }

    /// Whether a lease found on the share still holds, and whose it is.
    /// `None`: it ran out, or is what a dead worker of this user left on
    /// this PC, and may be taken.
    fn live(&self, found: &Found, now_ms: i64, offset: i64) -> Option<Holder> {
        let Some(l) = &found.lease else {
            let until = found.mtime_ms + UNREADABLE_MS;
            return (now_ms <= until).then(|| Holder { host: "another PC".into(), user: String::new(), pid: 0, until: (until - offset) / 1000 });
        };
        let same_user = l.host.eq_ignore_ascii_case(&self.me.host) && l.user.eq_ignore_ascii_case(&self.me.user);
        // Several workers of one user may run on a PC (readers); only one that is gone leaves its lease to the others.
        if same_user && l.pid != self.me.pid && !pid_alive(l.pid) {
            return None;
        }
        let until = l.expires(found.mtime_ms);
        (now_ms <= until + grace_ms(l.ttl)).then(|| Holder { host: l.host.clone(), user: l.user.clone(), pid: l.pid, until: (until - offset) / 1000 })
    }

    /// Take `<dir>\<name>.lease` for `ttl`. Without contention this is one
    /// file creation and one write.
    pub fn acquire(&self, name: &str, ttl: Duration) -> Acquire {
        let Some(path) = self.file_of(name) else { return Acquire::Unavailable(format!("{name:?} is not a lease name")) };
        match self.try_acquire(name, &path, ttl) {
            Ok(a) => a,
            Err(e) => Acquire::Unavailable(format!("{}: {e}", path.display())),
        }
    }

    fn try_acquire(&self, name: &str, path: &Path, ttl: Duration) -> io::Result<Acquire> {
        let offset = self.offset()?;
        let ttl_s = ttl.as_secs() + (ttl.subsec_nanos() > 0) as u64;
        let mut made_dir = false;
        let mut tries = 0;
        while tries < 4 {
            let started = Instant::now();
            match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut f) => {
                    let mut lease = Lease { path: path.to_path_buf(), name: name.to_string(), me: self.me.clone(), token: new_token(), ttl, ttl_s, offset, n: 0, good_until: started + ttl, lost: false, done: false };
                    if let Err(e) = f.write_all(&lease.content()) {
                        drop(f);
                        lease.done = true;
                        let _ = std::fs::remove_file(path);
                        return Err(e);
                    }
                    return Ok(Acquire::Mine(lease));
                }
                Err(e) if e.kind() == ErrorKind::NotFound && !made_dir => {
                    if let Some(up) = path.parent() {
                        std::fs::create_dir_all(up)?;
                    }
                    made_dir = true;
                    continue;
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                // Being removed by another PC this instant.
                Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(5 << tries));
                    continue;
                }
                Err(e) => return Err(e),
            }
            tries += 1;
            let found = match read_lease(path) {
                Ok(f) => f,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if let Some(h) = self.live(&found, local_ms() + offset, offset) {
                return Ok(Acquire::HeldBy(h));
            }
            take_away(path, &found);
        }
        // Lost every round: someone else has it by now.
        match self.holder(name) {
            Some(h) => Ok(Acquire::HeldBy(h)),
            None => Err(io::Error::other("the lease kept changing hands")),
        }
    }

    /// Who holds it now, if anyone (expired holders are not reported).
    pub fn holder(&self, name: &str) -> Option<Holder> {
        let found = read_lease(&self.file_of(name)?).ok()?;
        let offset = self.offset().ok()?;
        self.live(&found, local_ms() + offset, offset)
    }

    /// Delete lease files, here and in the sub-folders, that ran out more
    /// than ten minutes ago, and renamed-aside or temporary files older
    /// than that. Never touches a live lease. Returns how many were removed.
    pub fn sweep(&self) -> usize {
        let Ok(offset) = self.offset() else { return 0 };
        let mut removed = 0;
        self.sweep_dir(&self.dir, local_ms() + offset, 0, &mut removed);
        removed
    }

    fn sweep_dir(&self, dir: &Path, now_ms: i64, depth: usize, removed: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for ent in rd.flatten() {
            let Ok(md) = ent.metadata() else { continue };
            if md.is_dir() {
                if depth < MAX_DEPTH {
                    self.sweep_dir(&ent.path(), now_ms, depth + 1, removed);
                }
                continue;
            }
            // Nothing changed in the last ten minutes can have run out ten minutes ago.
            if !md.modified().map(ms_of).is_ok_and(|m| now_ms - m > SWEEP_MS) {
                continue;
            }
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.ends_with(".dead") || name.ends_with(".tmp") || name.ends_with(".probe") {
                *removed += std::fs::remove_file(ent.path()).is_ok() as usize;
            } else if name.ends_with(".lease") {
                let path = ent.path();
                let Ok(found) = read_lease(&path) else { continue };
                let until = found.lease.as_ref().map_or(found.mtime_ms, |l| l.expires(found.mtime_ms));
                if now_ms - until > SWEEP_MS {
                    *removed += take_away(&path, &found) as usize;
                }
            }
        }
    }
}

/// A lease this PC holds. Dropping it releases it if it is still ours.
#[derive(Debug)]
pub struct Lease {
    path: PathBuf,
    name: String,
    me: Identity,
    token: String,
    ttl: Duration,
    ttl_s: u64,
    /// Server clock minus ours, ms (for the `until` people read).
    offset: i64,
    n: u64,
    /// Our own clock's end of the term last written. Never later than the
    /// server's: the file is written after this is taken.
    good_until: Instant,
    lost: bool,
    /// Released or left: nothing more to do on drop.
    done: bool,
}

impl Lease {
    fn content(&self) -> Vec<u8> {
        let f = LeaseFile { host: self.me.host.clone(), user: self.me.user.clone(), pid: self.me.pid, token: self.token.clone(), until: (local_ms() + self.offset) / 1000 + self.ttl_s as i64, ttl: self.ttl_s, n: self.n };
        serde_json::to_vec(&f).unwrap_or_default()
    }

    fn is_ours(&self, found: &Found) -> bool {
        found.lease.as_ref().is_some_and(|l| l.token == self.token)
    }

    /// Push the expiry out by the original ttl. False when the lease was lost
    /// (expired and taken over): the caller must stop the work it guards.
    /// Also false once the term has run out without a renewal, taken over
    /// or not: another PC may be taking it this instant.
    pub fn renew(&mut self) -> bool {
        let started = Instant::now();
        if self.lost || self.done || started >= self.good_until {
            self.lost = true;
            return false;
        }
        match read_lease(&self.path) {
            Ok(found) if self.is_ours(&found) => {
                self.n += 1;
                if replace(&beside(&self.path, &self.token, "tmp"), &self.path, &self.content()).is_ok() {
                    self.good_until = started + self.ttl;
                }
                // Not written (the share is away): it is ours for the rest of the term we had.
                true
            }
            Ok(_) => {
                self.lost = true;
                false
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                self.lost = true;
                false
            }
            Err(_) => true,
        }
    }

    /// Give the lease back now.
    pub fn release(mut self) {
        self.give_back();
    }

    /// Let go without deleting the file: the lease simply runs out at its
    /// expiry, and nobody takes it before.
    pub fn leave(mut self) {
        self.done = true;
    }

    /// Let go, but keep the lease for `hold` from now (a document that was
    /// read stays its reader's until the reading is surely seen by every PC).
    /// Written only while the lease is still ours; otherwise it is left as
    /// it is. Older versions judge the term from the file the same way.
    pub fn leave_for(mut self, hold: Duration) {
        if !self.lost && Instant::now() < self.good_until
            && let Ok(found) = read_lease(&self.path)
            && self.is_ours(&found)
        {
            self.ttl_s = hold.as_secs().max(1);
            self.n += 1;
            let _ = replace(&beside(&self.path, &self.token, "tmp"), &self.path, &self.content());
        }
        self.done = true;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn give_back(&mut self) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        if let Ok(found) = read_lease(&self.path)
            && self.is_ours(&found)
        {
            take_away(&self.path, &found);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.give_back();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::scratch;

    fn pc(host: &str) -> Identity {
        Identity { host: host.into(), user: "u".into(), pid: std::process::id() }
    }

    fn mine(a: Acquire) -> Lease {
        match a {
            Acquire::Mine(l) => l,
            Acquire::HeldBy(h) => panic!("held by {h:?}"),
            Acquire::Unavailable(e) => panic!("unavailable: {e}"),
        }
    }

    fn held(a: Acquire) -> Holder {
        match a {
            Acquire::HeldBy(h) => h,
            Acquire::Mine(_) => panic!("got the lease"),
            Acquire::Unavailable(e) => panic!("unavailable: {e}"),
        }
    }

    const LONG: Duration = Duration::from_secs(60);

    #[test]
    fn one_holder_at_a_time() {
        let d = scratch("lease-one");
        let (a, b) = (Leases::new(&d, pc("PC-A")), Leases::new(&d, pc("PC-B")));
        let l = mine(a.acquire("walk-x", LONG));
        assert_eq!(l.name(), "walk-x");
        assert!(d.join("walk-x.lease").exists());
        let h = held(b.acquire("walk-x", LONG));
        assert_eq!((h.host.as_str(), h.user.as_str(), h.pid), ("PC-A", "u", std::process::id()));
        let now = local_ms() / 1000;
        assert!((now + 50..=now + 70).contains(&h.until), "until {} against now {now}", h.until);
        assert_eq!(b.holder("walk-x").map(|h| h.host), Some("PC-A".into()));
        held(a.acquire("walk-x", LONG));
        mine(b.acquire("walk-y", LONG)).release();
        l.release();
        assert!(!d.join("walk-x.lease").exists());
        assert!(b.holder("walk-x").is_none());
        // Dropping releases too.
        drop(mine(b.acquire("walk-x", LONG)));
        mine(a.acquire("walk-x", LONG));
        let leftovers: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_file_says_who_holds_it() {
        let d = scratch("lease-json");
        let a = Leases::new(&d, pc("PC-A"));
        let l = mine(a.acquire("compact", LONG));
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(d.join("compact.lease")).unwrap()).unwrap();
        assert_eq!(v["host"], "PC-A");
        assert_eq!(v["user"], "u");
        assert_eq!(v["pid"], std::process::id());
        assert_eq!(v["token"].as_str().unwrap().len(), 32);
        assert_eq!(v["ttl"], 60);
        assert!(v["until"].as_i64().unwrap() > 0);
        drop(l);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn renewing_keeps_it_past_its_first_term() {
        let d = scratch("lease-renew");
        let (a, b) = (Leases::new(&d, pc("PC-A")), Leases::new(&d, pc("PC-B")));
        let ttl = Duration::from_secs(5);
        let mut l = mine(a.acquire("walk-x", ttl));
        // Well past the term and the wait a taker adds to it.
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(500));
            assert!(l.renew());
            held(b.acquire("walk-x", ttl));
        }
        assert!(l.n >= 3);
        l.release();
        mine(b.acquire("walk-x", ttl));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn an_expired_lease_is_taken_over_and_its_holder_told() {
        let d = scratch("lease-expire");
        let (a, b) = (Leases::new(&d, pc("PC-A")), Leases::new(&d, pc("PC-B")));
        let ttl = Duration::from_secs(2);
        let mut first = mine(a.acquire("walk-x", ttl));
        held(b.acquire("walk-x", ttl));
        std::thread::sleep(Duration::from_millis(5500));
        assert!(b.holder("walk-x").is_none(), "expired holders are not reported");
        let second = mine(b.acquire("walk-x", LONG));
        assert!(!first.renew(), "the first holder must learn it lost the lease");
        // Dropping the lost lease must not remove the new holder's.
        drop(first);
        assert_eq!(held(a.acquire("walk-x", ttl)).host, "PC-B");
        drop(second);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_term_that_ran_out_is_not_renewed() {
        let d = scratch("lease-late");
        let a = Leases::new(&d, pc("PC-A"));
        let mut l = mine(a.acquire("walk-x", Duration::from_millis(300)));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!l.renew(), "too late: another PC may be taking it");
        assert!(!l.renew());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_lease_taken_away_makes_renew_fail() {
        let d = scratch("lease-lost");
        let a = Leases::new(&d, pc("PC-A"));
        let mut l = mine(a.acquire("walk-x", LONG));
        assert!(l.renew());
        std::fs::remove_file(d.join("walk-x.lease")).unwrap();
        assert!(!l.renew());
        let mut l = mine(a.acquire("walk-x", LONG));
        let other = LeaseFile { host: "PC-Z".into(), user: "u".into(), pid: 1, token: "f".repeat(32), until: 0, ttl: 60, n: 0 };
        std::fs::write(d.join("walk-x.lease"), serde_json::to_vec(&other).unwrap()).unwrap();
        assert!(!l.renew());
        drop(l);
        assert!(d.join("walk-x.lease").exists(), "another holder's lease is not ours to remove");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn what_a_dead_worker_of_this_user_left_is_taken() {
        let d = scratch("lease-leftover");
        // A process that has ended: its number is no longer running.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap()).arg("--list").stdout(std::process::Stdio::null()).spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        let crashed = Leases::new(&d, Identity { host: "PC-A".into(), user: "u".into(), pid: gone });
        std::mem::forget(mine(crashed.acquire("walk-x", LONG)));
        // Another PC, and another user of this PC, wait for it to run out.
        held(Leases::new(&d, pc("PC-B")).acquire("walk-x", LONG));
        held(Leases::new(&d, Identity { host: "PC-A".into(), user: "v".into(), pid: std::process::id() }).acquire("walk-x", LONG));
        let l = mine(Leases::new(&d, pc("pc-a")).acquire("walk-x", LONG));
        drop(l);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_running_worker_of_this_user_keeps_its_lease() {
        let d = scratch("lease-sibling");
        // A process that stays until it is killed.
        let mut child = std::process::Command::new("cmd").args(["/c", "pause"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).spawn().unwrap();
        let sibling = Leases::new(&d, Identity { host: "PC-A".into(), user: "u".into(), pid: child.id() });
        std::mem::forget(mine(sibling.acquire("doc/3f/9a1c", LONG)));
        assert_eq!(held(Leases::new(&d, pc("PC-A")).acquire("doc/3f/9a1c", LONG)).pid, child.id());
        let _ = child.kill();
        let _ = child.wait();
        mine(Leases::new(&d, pc("PC-A")).acquire("doc/3f/9a1c", LONG));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_file_that_is_not_a_lease() {
        let d = scratch("lease-garbage");
        std::fs::create_dir_all(&d).unwrap();
        let a = Leases::new(&d, pc("PC-A"));
        let path = d.join("walk-x.lease");
        // Fresh: someone is writing it this instant.
        std::fs::write(&path, b"{\"host\":").unwrap();
        let h = held(a.acquire("walk-x", LONG));
        assert_eq!(h.host, "another PC");
        assert!(a.holder("walk-x").is_some());
        // Old: damage, taken.
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(120)).unwrap();
        drop(f);
        assert!(a.holder("walk-x").is_none());
        let l = mine(a.acquire("walk-x", LONG));
        drop(l);
        // Empty, and a folder that is not there yet.
        std::fs::write(&path, b"").unwrap();
        held(a.acquire("walk-x", LONG));
        mine(Leases::new(d.join("deeper").join("still"), pc("PC-A")).acquire("walk-x", LONG));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_folder_that_cannot_be_used_is_unavailable() {
        let d = scratch("lease-unavailable");
        std::fs::create_dir_all(&d).unwrap();
        // A file where the folder should be.
        std::fs::write(d.join("leases"), b"x").unwrap();
        let a = Leases::new(d.join("leases"), pc("PC-A"));
        assert!(matches!(a.acquire("walk-x", LONG), Acquire::Unavailable(_)));
        assert!(a.holder("walk-x").is_none());
        assert_eq!(a.sweep(), 0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn names_may_use_sub_folders_and_nothing_else() {
        let d = scratch("lease-names");
        let a = Leases::new(&d, pc("PC-A"));
        let l = mine(a.acquire(r"doc\3f\9a1c0e", LONG));
        assert!(d.join("doc").join("3f").join("9a1c0e.lease").exists());
        assert_eq!(l.name(), r"doc\3f\9a1c0e");
        held(Leases::new(&d, pc("PC-B")).acquire("doc/3f/9a1c0e", LONG));
        mine(a.acquire("walk-10.0.0.5~project", LONG));
        for bad in ["", "..", r"..\x", r"doc\..\x", r"a\\b", r"\a", "a/", "a:b", "a*", "a?b", "a|b", "a\"b", "a<b", "a\nb", "a.", r"a\b\c\d\e", "."] {
            assert!(matches!(a.acquire(bad, LONG), Acquire::Unavailable(_)), "{bad:?} must be refused");
            assert!(a.holder(bad).is_none());
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_left_lease_runs_out_by_itself() {
        let d = scratch("lease-leave");
        let (a, b) = (Leases::new(&d, pc("PC-A")), Leases::new(&d, pc("PC-B")));
        let ttl = Duration::from_secs(2);
        mine(a.acquire("doc/aa/1", ttl)).leave();
        assert!(d.join("doc").join("aa").join("1.lease").exists(), "leaving does not delete it");
        assert_eq!(held(b.acquire("doc/aa/1", ttl)).host, "PC-A");
        held(a.acquire("doc/aa/1", ttl));
        std::thread::sleep(Duration::from_millis(5500));
        mine(b.acquire("doc/aa/1", ttl));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_lease_left_for_longer_holds_past_its_term() {
        let d = scratch("lease-leave-for");
        let (a, b) = (Leases::new(&d, pc("PC-A")), Leases::new(&d, pc("PC-B")));
        let ttl = Duration::from_secs(1);
        mine(a.acquire("doc/bb/1", ttl)).leave_for(Duration::from_secs(3600));
        std::thread::sleep(Duration::from_millis(3500));
        let h = held(b.acquire("doc/bb/1", ttl));
        assert_eq!(h.host, "PC-A");
        assert!(h.until > local_ms() / 1000 + 3000, "held for the hour: {h:?}");
        // As an older version sees it: the term is in the file.
        let f: LeaseFile = serde_json::from_slice(&std::fs::read(d.join("doc").join("bb").join("1.lease")).unwrap()).unwrap();
        assert_eq!(f.ttl, 3600);
        let _ = std::fs::remove_dir_all(d);
    }

    fn age(path: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
    }

    #[test]
    fn sweeping_removes_only_what_ran_out_long_ago() {
        let d = scratch("lease-sweep");
        let a = Leases::new(&d, pc("PC-A"));
        let live = mine(a.acquire("walk-live", LONG));
        // Ran out 15 minutes ago, in a sub-folder; ran out 5 minutes ago.
        mine(a.acquire("doc/3f/old", LONG)).leave();
        age(&d.join("doc").join("3f").join("old.lease"), 16 * 60);
        mine(a.acquire("doc/3f/recent", LONG)).leave();
        age(&d.join("doc").join("3f").join("recent.lease"), 6 * 60);
        // Long term: written 20 minutes ago but good for an hour.
        mine(a.acquire("long", Duration::from_secs(3600))).leave();
        age(&d.join("long.lease"), 20 * 60);
        for (name, secs) in [("x.abc.dead", 11 * 60), ("x.abc.tmp", 11 * 60), ("clock-PC-1-2.probe", 11 * 60), ("y.abc.dead", 60), ("damaged.lease", 11 * 60), ("notes.txt", 11 * 60)] {
            std::fs::write(d.join(name), b"x").unwrap();
            age(&d.join(name), secs);
        }
        assert_eq!(a.sweep(), 5);
        let mut left: Vec<String> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["doc", "long.lease", "notes.txt", "walk-live.lease", "y.abc.dead"]);
        let sub: Vec<String> = std::fs::read_dir(d.join("doc").join("3f")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(sub, ["recent.lease"]);
        assert_eq!(a.sweep(), 0);
        drop(live);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn many_threads_never_hold_it_together() {
        let d = scratch("lease-threads");
        let inside = std::sync::atomic::AtomicU32::new(0);
        let got = std::sync::atomic::AtomicU32::new(0);
        std::thread::scope(|s| {
            for i in 0..4 {
                let (d, inside, got) = (&d, &inside, &got);
                s.spawn(move || {
                    let me = Leases::new(d, pc(&format!("PC-{i}")));
                    for _ in 0..10 {
                        loop {
                            match me.acquire("walk-x", LONG) {
                                Acquire::Mine(l) => {
                                    assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0, "two holders at once");
                                    std::thread::sleep(Duration::from_millis(3));
                                    inside.fetch_sub(1, Ordering::SeqCst);
                                    got.fetch_add(1, Ordering::SeqCst);
                                    l.release();
                                    break;
                                }
                                Acquire::HeldBy(_) => std::thread::sleep(Duration::from_millis(4)),
                                Acquire::Unavailable(e) => panic!("unavailable: {e}"),
                            }
                        }
                    }
                });
            }
        });
        assert_eq!(got.load(Ordering::SeqCst), 40);
        let _ = std::fs::remove_dir_all(d);
    }
}
