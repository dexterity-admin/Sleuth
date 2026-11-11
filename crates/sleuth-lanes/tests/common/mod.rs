//! Shared by the tests that run real reader processes (`examples/echo_child.rs`).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sleuth_lanes::{Event, ProcessGroup, Spawn, Workers};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct Job {
    pub op: String,
    pub text: String,
    pub ms: u64,
    pub mb: usize,
}

impl Job {
    pub fn op(op: &str) -> Job {
        Job { op: op.to_string(), ..Default::default() }
    }
    pub fn echo(text: &str) -> Job {
        Job { op: "echo".to_string(), text: text.to_string(), ..Default::default() }
    }
    pub fn sleep(ms: u64) -> Job {
        Job { op: "sleep".to_string(), ms, ..Default::default() }
    }
}

#[derive(Deserialize, Debug)]
pub struct Reply {
    pub text: String,
    pub pid: u32,
    pub init: Value,
}

/// The built example, looked for beside this test program and then where
/// `CARGO_TARGET_DIR` and the manifest say the target folder is.
pub fn child_exe() -> Option<PathBuf> {
    let mut folders = Vec::new();
    if let Ok(me) = std::env::current_exe() {
        // <target>/<profile>/deps/<test>.exe
        if let Some(profile) = me.parent().and_then(|deps| deps.parent()) {
            folders.push(profile.to_path_buf());
        }
    }
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("target"));
    folders.push(target.join("debug"));
    folders.push(target.join("release"));
    let found = folders.iter().map(|f| f.join("examples").join("echo_child.exe")).find(|p| p.is_file());
    if found.is_none() {
        eprintln!("SKIPPED: examples/echo_child.exe is not built. Run `cargo build -p sleuth-lanes --examples` first. Looked in {folders:?}");
    }
    found
}

pub fn alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid) else { return false };
        let running = WaitForSingleObject(process, 0) == WAIT_TIMEOUT;
        let _ = CloseHandle(process);
        running
    }
}

pub fn priority_class(pid: u32) -> u32 {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetPriorityClass, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return 0 };
        let class = GetPriorityClass(process);
        let _ = CloseHandle(process);
        class
    }
}

/// Wait up to 10 s for every one of these processes to have gone.
pub fn all_gone(pids: &[u32]) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    while pids.iter().any(|p| alive(*p)) {
        if Instant::now() > until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// One lane, "lane", of echo children, and the events seen so far.
pub struct Rig {
    pub group: Arc<ProcessGroup>,
    pub w: Workers<Job, Reply>,
    pub seen: Vec<Event<Job, Reply>>,
}

/// Readers run at idle priority: on a PC busy with other work they get very little CPU.
const PATIENCE: Duration = Duration::from_secs(600);

impl Rig {
    /// `None` (after saying why) when the example is not built.
    pub fn new(workers: u32, init: Value, args: &[&str]) -> Option<Rig> {
        let exe = child_exe()?;
        let group = Arc::new(ProcessGroup::new().expect("job object"));
        let mut w = Workers::new(group.clone());
        w.add_lane("lane", Spawn::Process { exe, args: args.iter().map(|a| a.to_string()).collect(), env: Vec::new() }, init);
        w.set_target("lane", workers);
        Some(Rig { group, w, seen: Vec::new() })
    }

    pub fn pump(&mut self, wait: Duration) {
        for (lane, event) in self.w.poll(wait) {
            assert_eq!(lane, "lane");
            self.seen.push(event);
        }
    }

    /// Submit, waiting for a worker to be ready and idle.
    pub fn give(&mut self, mut job: Job, timeout: Duration) {
        let until = Instant::now() + PATIENCE;
        loop {
            match self.w.submit("lane", job, timeout) {
                Ok(()) => return,
                Err(back) => job = back,
            }
            assert!(Instant::now() < until, "no worker took the job: {:?}", self.w.lane_error("lane"));
            self.pump(Duration::from_millis(20));
        }
    }

    /// The next event, oldest first.
    pub fn event(&mut self) -> Event<Job, Reply> {
        let until = Instant::now() + PATIENCE;
        while self.seen.is_empty() {
            assert!(Instant::now() < until, "no event came");
            self.pump(Duration::from_millis(50));
        }
        self.seen.remove(0)
    }

    /// The next event, which must be an answer.
    pub fn done(&mut self) -> Reply {
        match self.event() {
            Event::Done { reply, .. } => reply,
            Event::Crashed { job, detail } => panic!("{} crashed: {detail}", job.op),
            Event::TimedOut { job, .. } => panic!("{} timed out", job.op),
        }
    }

    /// The next event, which must be a crash: (the job's op, the detail).
    pub fn crash(&mut self) -> (String, String) {
        match self.event() {
            Event::Crashed { job, detail } => (job.op, detail),
            Event::Done { job, reply, .. } => panic!("{} answered {:?} instead of crashing", job.op, reply.text),
            Event::TimedOut { job, millis } => panic!("{} timed out after {millis} ms instead of crashing", job.op),
        }
    }

    /// Wait until every worker has started: all `n` hold a job at the same time.
    pub fn warm(&mut self, n: u32) {
        let until = Instant::now() + PATIENCE;
        loop {
            while self.w.submit("lane", Job::sleep(300), Duration::from_secs(30)).is_ok() {}
            if self.w.counts("lane").1 >= n {
                break;
            }
            assert!(Instant::now() < until, "the workers did not start: {:?}", self.w.lane_error("lane"));
            self.pump(Duration::from_millis(20));
        }
        while !self.w.in_flight().is_empty() {
            assert!(Instant::now() < until, "the warm-up jobs did not finish");
            self.pump(Duration::from_millis(50));
        }
        self.pump(Duration::ZERO);
        assert!(self.seen.iter().all(|e| matches!(e, Event::Done { .. })), "a warm-up job failed");
        self.seen.clear();
    }
}
