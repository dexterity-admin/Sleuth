//! Being a good neighbour on the user's PC: background thread mode, CPU load,
//! battery and idle detection.

#[cfg(windows)]
use windows::Win32::Foundation::FILETIME;
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, GetProcessTimes, GetSystemTimes, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY_BELOW_NORMAL};

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

impl HalfTime {
    pub fn start() -> HalfTime {
        let turn = BACKGROUND.lock().unwrap_or_else(|p| p.into_inner());
        HalfTime(std::time::Instant::now(), Some(turn))
    }
}

impl Drop for HalfTime {
    fn drop(&mut self) {
        let worked = self.0.elapsed();
        // The next job may start while this one rests.
        self.1.take();
        std::thread::sleep(worked.min(std::time::Duration::from_secs(2)));
    }
}
