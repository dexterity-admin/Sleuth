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
