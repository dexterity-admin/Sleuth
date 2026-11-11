//! The Windows job object that holds every reader process.

use anyhow::Result;

#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOBOBJECTINFOCLASS, JobObjectBasicAccountingInformation,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
};

/// A Windows job object holding every reader process: they die with the worker
/// (KILL_ON_JOB_CLOSE), and their CPU time is known as a whole.
pub struct ProcessGroup {
    #[cfg(windows)]
    job: HANDLE,
}

// A job handle may be used from any thread; the kernel does the locking.
unsafe impl Send for ProcessGroup {}
unsafe impl Sync for ProcessGroup {}

/// The process ids in a job; the system fills in as many as fit.
#[cfg(windows)]
#[repr(C)]
struct ProcessIds {
    assigned: u32,
    in_list: u32,
    ids: [usize; 256],
}

impl ProcessGroup {
    pub fn new() -> Result<ProcessGroup> {
        #[cfg(windows)]
        unsafe {
            let group = ProcessGroup { job: CreateJobObjectW(None, windows::core::PCWSTR::null())? };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            // Die on unhandled exception: a process in the group that crashes
            // ends with the exception code, without the system's error reporter
            // (which would run at the readers' idle priority and take its time).
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
            SetInformationJobObject(group.job, JobObjectExtendedLimitInformation, &info as *const _ as *const core::ffi::c_void, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32)?;
            Ok(group)
        }
        #[cfg(not(windows))]
        Ok(ProcessGroup {})
    }

    /// Cumulative user + kernel time of all processes ever in the group, 100 ns units.
    pub fn cpu_100ns(&self) -> u64 {
        #[cfg(windows)]
        {
            let times = self.query::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>(JobObjectBasicAccountingInformation);
            times.map(|a| (a.TotalUserTime.max(0) as u64).saturating_add(a.TotalKernelTime.max(0) as u64)).unwrap_or(0)
        }
        #[cfg(not(windows))]
        0
    }

    /// Peak memory of the largest process seen, MB (JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    /// PeakProcessMemoryUsed, which counts committed memory), 0 if unknown.
    pub fn peak_process_mb(&self) -> u64 {
        #[cfg(windows)]
        {
            let limits = self.query::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(JobObjectExtendedLimitInformation);
            limits.map(|x| (x.PeakProcessMemoryUsed as u64) >> 20).unwrap_or(0)
        }
        #[cfg(not(windows))]
        0
    }

    /// Physical memory the group's live processes hold right now, MB.
    /// The system's "available memory" has this already taken out; a caller
    /// that sizes the group from available memory adds it back, or every
    /// worker it starts looks like a reason to stop one.
    pub fn working_set_mb(&self) -> u64 {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
            use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
            let mut list = ProcessIds { assigned: 0, in_list: 0, ids: [0; 256] };
            // "More data" still fills the list as far as it goes.
            let _ = QueryInformationJobObject(Some(self.job), JobObjectBasicProcessIdList, &mut list as *mut _ as *mut core::ffi::c_void, std::mem::size_of::<ProcessIds>() as u32, None);
            let mut bytes = 0u64;
            for pid in list.ids.iter().take((list.in_list as usize).min(list.ids.len())) {
                let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, *pid as u32) else { continue };
                let mut m = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
                if K32GetProcessMemoryInfo(process, &mut m, m.cb).as_bool() {
                    bytes += m.WorkingSetSize as u64;
                }
                let _ = CloseHandle(process);
            }
            bytes >> 20
        }
        #[cfg(not(windows))]
        0
    }

    #[cfg(windows)]
    fn query<T: Default>(&self, class: JOBOBJECTINFOCLASS) -> Option<T> {
        let mut out = T::default();
        unsafe { QueryInformationJobObject(Some(self.job), class, &mut out as *mut T as *mut core::ffi::c_void, std::mem::size_of::<T>() as u32, None).ok()? };
        Some(out)
    }

    /// Put a just-started process into the group. Fails on Windows 7 when the
    /// worker itself runs inside a job (nested jobs came with Windows 8).
    #[cfg(windows)]
    pub(crate) fn assign(&self, process: std::os::windows::io::RawHandle) -> Result<()> {
        unsafe { AssignProcessToJobObject(self.job, HANDLE(process))? };
        Ok(())
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Closing the last handle is what kills whatever is still in the job.
        #[cfg(windows)]
        unsafe {
            let _ = CloseHandle(self.job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_group_reads_zero() {
        let g = ProcessGroup::new().unwrap();
        assert_eq!((g.cpu_100ns(), g.peak_process_mb(), g.working_set_mb()), (0, 0, 0));
        fn shared<T: Send + Sync>(_: &T) {}
        shared(&g);
    }
}
