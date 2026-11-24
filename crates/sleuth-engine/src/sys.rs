//! Being a good neighbour on the user's PC: background thread mode, CPU load,
//! battery and idle detection.

#[cfg(windows)]
use windows::Win32::Foundation::FILETIME;
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, GetProcessTimes, GetSystemTimes, GetThreadTimes, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY_BELOW_NORMAL};

/// Lower this thread's CPU, I/O and memory priority (as Everything does for
/// its monitor threads).
pub fn enter_background_mode() {
    #[cfg(windows)]
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}

/// Lower only this thread's CPU priority. For folder listing: background
/// mode also drops disk priority to the lowest, and on a busy hard disk a
/// folder of a few thousand files then waits for minutes.
pub fn enter_low_priority() {
    #[cfg(windows)]
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

#[cfg(windows)]
fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// Measures how busy the PC is with work other than Sleuth.
#[derive(Default)]
pub struct CpuMeter {
    prev: Option<(u64, u64, u64)>,
}

impl CpuMeter {
    /// Percentage of total CPU used by other programs since the last call.
    pub fn others_percent(&mut self) -> f32 {
        #[cfg(windows)]
        unsafe {
            let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
            if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_err() {
                return 0.0;
            }
            let (mut c, mut e, mut pk, mut pu) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut pk, &mut pu);
            // System kernel time includes idle time.
            let total = ft(kernel) + ft(user);
            let busy = total - ft(idle);
            let mine = ft(pk) + ft(pu);
            let now = (total, busy, mine);
            let pct = match self.prev {
                Some((t0, b0, m0)) if now.0 > t0 => {
                    let dt = (now.0 - t0) as f32;
                    let others = (now.1.saturating_sub(b0)).saturating_sub(now.2.saturating_sub(m0)) as f32;
                    (others / dt * 100.0).clamp(0.0, 100.0)
                }
                _ => 0.0,
            };
            self.prev = Some(now);
            pct
        }
        #[cfg(not(windows))]
        0.0
    }
}

pub fn on_battery() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut s = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&mut s).is_ok() {
            return s.ACLineStatus == 0;
        }
    }
    false
}

/// Seconds since the last keyboard or mouse input.
pub fn idle_seconds() -> u64 {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::SystemInformation::GetTickCount;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        if GetLastInputInfo(&mut li).as_bool() {
            return (GetTickCount().wrapping_sub(li.dwTime) / 1000) as u64;
        }
    }
    0
}

/// Free bytes on the disk holding `dir` (`u64::MAX` when it cannot be told).
pub fn free_bytes(dir: &std::path::Path) -> u64 {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut free = 0u64;
        let wide = windows::core::HSTRING::from(dir.as_os_str());
        if GetDiskFreeSpaceExW(&wide, Some(&mut free), None, None).is_ok() {
            return free;
        }
    }
    let _ = dir;
    u64::MAX
}

/// Whether the disk behind a drive letter (`"Q:"`) has no seek penalty (an
/// SSD), as Windows reports it. False when it cannot be told.
pub fn is_ssd(letter: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
        use windows::Win32::System::IO::DeviceIoControl;
        use windows::Win32::System::Ioctl::{DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty};
        let name = windows::core::HSTRING::from(format!(r"\\.\{}", letter.trim_end_matches('\\')));
        // No access rights asked for: enough to query, and needs no administrator.
        let Ok(h) = CreateFileW(&name, 0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None) else { return false };
        let query = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceSeekPenaltyProperty, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
        let mut out = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
        let mut got = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const core::ffi::c_void),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(&mut out as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            Some(&mut got),
            None,
        )
        .is_ok();
        let _ = CloseHandle(h);
        return ok && !bool::from(out.IncursSeekPenalty);
    }
    #[cfg(not(windows))]
    {
        let _ = letter;
        false
    }
}

/// Where the copy of the office index goes when nobody chose: the local
/// disk that suits best. An SSD before a hard disk, a disk with 100 GB free
/// before a fuller one, a second disk before the Windows disk, then the most
/// free space. `fixed` are this PC's local drive letters (`"C:"`); `data` is
/// Sleuth's own data folder, used when the Windows disk is the choice.
pub fn pick_index_dir(data: &std::path::Path, fixed: &[String]) -> std::path::PathBuf {
    const ROOMY: u64 = 100 << 30;
    const ENOUGH: u64 = 20 << 30;
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()).to_uppercase();
    let best = fixed
        .iter()
        .map(|l| l.trim_end_matches('\\').to_uppercase())
        .filter(|l| l.len() == 2)
        .map(|l| {
            let free = free_bytes(std::path::Path::new(&format!("{l}\\")));
            (is_ssd(&l), free != u64::MAX && free >= ROOMY, l != system, free, l)
        })
        .filter(|(_, _, _, free, _)| *free != u64::MAX && *free >= ENOUGH)
        .max();
    match best {
        Some((_, _, true, _, letter)) => std::path::PathBuf::from(format!("{letter}\\Sleuth-index")),
        _ => data.join("index"),
    }
}

/// Makes the thread that holds it work half the time: when dropped, it
/// sleeps for as long as it was held (two seconds at most).
///
/// Only one thread holds it at a time: the worker's background jobs (the
/// full pass of a search, the monitor's counts, counting what waits, working
/// out which readings are current) take turns, so together they stay under
/// half a core however many are due at once. Run side by side they added up
/// to a fifth of an 8-core PC (24 Nov 2026).
pub struct HalfTime(std::time::Instant, Option<std::sync::MutexGuard<'static, ()>>);

static BACKGROUND: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---- the processor a search may take ------------------------------------------
//
// A desktop's rule is that Sleuth stays under a fifth of the processor in
// any second. Readers have their own ceiling (a job object) and the worker's
// upkeep waits while a search runs, so what is left to hold is the search
// itself: its threads draw processor time from one allowance. A search that
// needs little (a tag, a number: under a core-second) is not slowed at all;
// one that needs more gets the allowance at once and the rest at a steady
// rate, on however many threads.

static GOVERNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Two allowances: `[0]` for the full pass (nobody waits for it), `[1]` for
/// the first screen of a search (file names, then the quick pass). With one
/// allowance for both, a search began with what the full pass of the one
/// before had overdrawn, and a two-word search showed its first rows after
/// 12 s (24 Nov 2026).
static ALLOWANCE: std::sync::Mutex<[Option<(f64, std::time::Instant)>; 2]> = std::sync::Mutex::new([None, None]);
/// Searches busy with their first screen.
static FIRST_SCREENS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Held while a search works on its first screen: every search thread then
/// draws from the first screen's allowance (a full pass of another search
/// too, so that the two together keep to it).
pub struct FirstScreen(());

impl FirstScreen {
    pub fn start() -> FirstScreen {
        FIRST_SCREENS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        FirstScreen(())
    }
}

impl Drop for FirstScreen {
    fn drop(&mut self) {
        FIRST_SCREENS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Switch the allowance on (a desktop) or off (the office server, tests).
pub fn govern(on: bool) {
    GOVERNED.store(on, std::sync::atomic::Ordering::SeqCst);
}

pub fn governed() -> bool {
    GOVERNED.load(std::sync::atomic::Ordering::SeqCst)
}

/// (core-seconds that may be taken at once, core-seconds given back per
/// second). For the first screen together 0.16 of the cores in any one
/// second (1.3 of 8), which with the readers' 1 % during a search stays under the
/// fifth a desktop allows; the full pass, alone, takes 0.075 of the cores
/// (0.6 of 8) steadily.
fn allowance(first_screen: bool) -> (f64, f64) {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) as f64;
    if first_screen { (cores * 0.10, cores * 0.06) } else { (cores * 0.05, cores * 0.075) }
}

/// Take `core_seconds` of processor time from the searches' allowance, after
/// the work was done; sleeps for as long as it takes the allowance to make
/// up what is overdrawn.
pub fn charge(core_seconds: f64) {
    charge_and(core_seconds, true);
}

/// As [`charge`], and with `wait` false without sleeping: for the last step
/// before rows are sent, which later work makes up for.
pub fn charge_and(core_seconds: f64, wait: bool) {
    if !governed() || core_seconds <= 0.0 {
        return;
    }
    let first = FIRST_SCREENS.load(std::sync::atomic::Ordering::SeqCst) > 0;
    let (most, rate) = allowance(first);
    let owed = {
        let mut g = ALLOWANCE.lock().unwrap_or_else(|p| p.into_inner());
        let now = std::time::Instant::now();
        let (had, at) = g[first as usize].unwrap_or((most, now));
        // Never more than five seconds behind, whatever was taken.
        let have = ((had + now.duration_since(at).as_secs_f64() * rate).min(most) - core_seconds).max(-5.0 * rate);
        g[first as usize] = Some((have, now));
        if have < 0.0 { -have / rate } else { 0.0 }
    };
    let wait = if wait { owed } else { 0.0 };
    if wait > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(wait.min(5.0)));
    }
}

/// Processor time this thread has used.
fn thread_cpu() -> f64 {
    #[cfg(windows)]
    unsafe {
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        if GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u).is_ok() {
            let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
            return t(k) + t(u);
        }
    }
    0.0
}

/// Charges the processor time its thread used while it was held.
pub struct SearchCpu(f64, bool);

impl SearchCpu {
    pub fn start() -> SearchCpu {
        SearchCpu(if governed() { thread_cpu() } else { 0.0 }, true)
    }

    /// Charges without making the thread wait.
    pub fn unpaced() -> SearchCpu {
        SearchCpu(if governed() { thread_cpu() } else { 0.0 }, false)
    }
}

impl Drop for SearchCpu {
    fn drop(&mut self) {
        if governed() {
            charge_and(thread_cpu() - self.0, self.1);
        }
    }
}

/// Threads for a scan of the file lists by a search: one on a desktop (a
/// scan on every core is over the rule in a quarter of a second), the
/// caller's choice elsewhere.
pub fn scan_threads(otherwise: usize) -> usize {
    if governed() { 1 } else { otherwise }
}

/// Full passes of searches under way: the one background job somebody is
/// waiting for.
static SEARCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Held by the full pass of a search. The other background jobs do not
/// start a turn while one is held, and the long ones stop between two
/// pieces of their work ([`rest_while_searching`]).
///
/// The full pass used to take a turn per part like every other job. A job
/// that keeps one turn for long (working out which readings are current,
/// counting what waits) then stood between every two parts of it, and a
/// pass of 4 s took 25 to 75 s (24 Nov 2026).
pub struct SearchTurn(());

impl SearchTurn {
    pub fn start() -> SearchTurn {
        SEARCHES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        SearchTurn(())
    }
}

impl Drop for SearchTurn {
    fn drop(&mut self) {
        SEARCHES.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Wait while the full pass of a search runs (half a minute at most: a
/// search must not stop the upkeep for good). Not for the search's own thread.
pub fn rest_while_searching() {
    let since = std::time::Instant::now();
    while SEARCHES.load(std::sync::atomic::Ordering::SeqCst) > 0 && since.elapsed() < std::time::Duration::from_secs(30) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

impl HalfTime {
    pub fn start() -> HalfTime {
        rest_while_searching();
        let turn = BACKGROUND.lock().unwrap_or_else(|p| p.into_inner());
        HalfTime(std::time::Instant::now(), Some(turn))
    }

    /// Half the time without waiting for a turn: the full pass of a search,
    /// which holds a [`SearchTurn`] instead.
    pub fn alone() -> HalfTime {
        HalfTime(std::time::Instant::now(), None)
    }
}

impl Drop for HalfTime {
    fn drop(&mut self) {
        let worked = self.0.elapsed();
        // The next job may start while this one rests.
        if self.1.take().is_some() && worked > std::time::Duration::from_secs(5) {
            tracing::info!("background: {} kept its turn for {} ms", std::thread::current().name().unwrap_or("a job"), worked.as_millis());
        }
        std::thread::sleep(worked.min(std::time::Duration::from_secs(2)));
    }
}
