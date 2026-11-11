//! What the PC is doing right now: CPU used by other programs, time since the
//! last key press, battery, free memory. The governor decides from these.

#[cfg(windows)]
use windows::Win32::Foundation::FILETIME;

#[cfg(windows)]
fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// One reading of the clocks, all in 100 ns units.
#[derive(Clone, Copy)]
struct Sample {
    /// CPU time of all cores together, busy or not.
    total: u64,
    /// The part of `total` in which something ran.
    busy: u64,
    /// The part of `busy` that was this process and its reader processes.
    ours: u64,
}

/// System-wide CPU use not caused by this process or by our reader processes.
#[derive(Default)]
pub struct CpuMeter {
    prev: Option<Sample>,
}

impl CpuMeter {
    pub fn new() -> CpuMeter {
        CpuMeter::default()
    }

    /// Percent (0..100, of all cores) used by everything else since the last call.
    /// `ours_100ns` is the cumulative CPU time (100 ns units) of the reader processes
    /// (from `ProcessGroup::cpu_100ns`), which is subtracted along with this process's own.
    /// The first call has nothing to compare with and answers 0.
    pub fn others_percent(&mut self, ours_100ns: u64) -> f32 {
        let Some(now) = sample(ours_100ns) else { return 0.0 };
        let pct = self.prev.map(|prev| others(prev, now)).unwrap_or(0.0);
        self.prev = Some(now);
        pct
    }
}

/// The share of the time between two samples that other programs used.
/// The clocks are read one after another and reader processes come and go,
/// so "ours" can come out larger than "busy": that is 0, never negative.
fn others(prev: Sample, now: Sample) -> f32 {
    let dt = now.total.saturating_sub(prev.total);
    if dt == 0 {
        return 0.0;
    }
    let busy = now.busy.saturating_sub(prev.busy);
    let ours = now.ours.saturating_sub(prev.ours);
    (busy.saturating_sub(ours) as f64 / dt as f64 * 100.0).clamp(0.0, 100.0) as f32
}

fn sample(ours_100ns: u64) -> Option<Sample> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()?;
        let (mut c, mut e, mut pk, mut pu) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut pk, &mut pu);
        // System kernel time includes idle time.
        let total = ft(kernel) + ft(user);
        Some(Sample { total, busy: total.saturating_sub(ft(idle)), ours: (ft(pk) + ft(pu)).saturating_add(ours_100ns) })
    }
    #[cfg(not(windows))]
    {
        let _ = ours_100ns;
        None
    }
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

/// Available physical memory, MB. 0 when it cannot be read, which the
/// governor treats as "no room": one OCR worker.
pub fn free_ram_mb() -> u64 {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        if GlobalMemoryStatusEx(&mut m).is_ok() {
            return m.ullAvailPhys >> 20;
        }
    }
    0
}

/// Logical processors this process may use.
pub fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn others_is_what_is_left_after_ours() {
        let a = Sample { total: 1_000, busy: 100, ours: 50 };
        // 1000 units pass on all cores: 600 busy, of which 200 ours.
        let b = Sample { total: 2_000, busy: 700, ours: 250 };
        assert_eq!(others(a, b), 40.0);
        assert_eq!(others(a, a), 0.0, "no time passed");
    }

    #[test]
    fn a_large_ours_never_goes_negative() {
        let a = Sample { total: 1_000, busy: 100, ours: 0 };
        let b = Sample { total: 2_000, busy: 700, ours: u64::MAX };
        assert_eq!(others(a, b), 0.0);
        // Clocks that ran backwards (a sample taken out of order) are also 0.
        assert_eq!(others(b, a), 0.0);
        let all = Sample { total: 2_000, busy: 5_000, ours: 0 };
        assert_eq!(others(a, all), 100.0, "never above 100");
    }

    #[test]
    fn live_meter_stays_in_range() {
        let mut m = CpuMeter::new();
        assert_eq!(m.others_percent(0), 0.0, "the first call has no interval");
        let mut x = 0u64;
        for i in 0..20_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        std::thread::sleep(std::time::Duration::from_millis(150));
        let p = m.others_percent(0);
        assert!((0.0..=100.0).contains(&p), "{p}");
        std::thread::sleep(std::time::Duration::from_millis(50));
        // More reader time than the PC has had since it started: still not negative.
        let p = m.others_percent(u64::MAX / 2);
        assert_eq!(p, 0.0);
    }

    #[test]
    fn readings_are_sane() {
        assert!(cores() >= 1);
        assert!(free_ram_mb() > 0);
        let _ = (idle_seconds(), on_battery());
    }
}
