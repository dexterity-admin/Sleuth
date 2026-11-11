//! The worker's side: lanes of reader processes, fed one job at a time.
//!
//! Everything here is driven from one thread, through `poll`. Each reader
//! process has two small threads of its own: one writes its stdin (so a slow
//! or stuck child never holds up the caller), one reads its stdout and turns
//! the lines into messages on a channel shared by all lanes. A closed stdout
//! is how a dead reader is noticed.
//!
//! A reader gets no job before it has said it is ready (it started, and it
//! understood the settings). So a reader program that cannot start shows up
//! as a lane error, never as a pile of files blamed for crashing it.

use crate::group::ProcessGroup;
use crate::wire;
use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// This many failed starts in a row within `START_WINDOW` and the lane stops trying for `START_PAUSE`.
const START_FAILS: u32 = 5;
const START_WINDOW: Duration = Duration::from_secs(60);
const START_PAUSE: Duration = Duration::from_secs(5 * 60);
/// A reader that has not said it is ready after this long is stuck.
const START_LIMIT: Duration = Duration::from_secs(600);
/// How long a reader told to stop (stdin closed) may take before it is killed.
const STOP_LIMIT: Duration = Duration::from_secs(10);
/// The same at shutdown, where someone is waiting.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(2);
/// `poll` looks at stuck and stopping readers at least this often.
const HOUSEKEEPING: Duration = Duration::from_secs(1);

/// What runs a job inside this process: the settings of the lane and the job in, the reply out.
pub type Handler<J, R> = dyn Fn(&Value, J) -> R + Send + Sync;

pub enum Spawn<J, R> {
    /// Start `exe args...` per worker; jobs go to its stdin, replies come from its stdout, one JSON object per line.
    Process { exe: PathBuf, args: Vec<String>, env: Vec<(String, String)> },
    /// Run jobs on threads in this process (tests, or a host program that cannot re-run itself). No crash isolation.
    InProcess(Arc<Handler<J, R>>),
}

pub enum Event<J, R> {
    Done { job: J, reply: R, millis: u64 },
    /// The worker process died while holding this job (the file crashed its reader).
    Crashed { job: J, detail: String },
    /// The job ran past its timeout; its worker was killed.
    TimedOut { job: J, millis: u64 },
}

/// What the reading threads tell the owner of the lanes.
enum Msg<R> {
    Ready { worker: u64 },
    Reply { worker: u64, id: u64, reply: R, at: Instant },
    /// The reader is alive but could not take the job or write its answer.
    Refused { worker: u64, id: u64, error: String },
    /// The reader's output ended (empty `detail`), or made no sense.
    Closed { worker: u64, detail: String },
}

struct Busy<J> {
    id: u64,
    job: J,
    started: Instant,
    /// `None`: a timeout too long to ever arrive.
    deadline: Option<Instant>,
}

enum Kind<J> {
    /// Dropping `stdin` closes the child's stdin, which is how it is asked to stop.
    Process { child: Child, stdin: Sender<Vec<u8>> },
    /// Dropping `jobs` ends the thread after the job it is on.
    Thread { jobs: Sender<(u64, J)> },
}

struct Worker<J> {
    id: u64,
    kind: Kind<J>,
    /// Which settings it was started with.
    init_gen: u64,
    started: Instant,
    ready: bool,
    /// Jobs it has been given.
    given: u64,
    busy: Option<Busy<J>>,
}

struct Lane<J, R> {
    name: String,
    spawn: Spawn<J, R>,
    init: Arc<Value>,
    init_gen: u64,
    target: u32,
    workers: Vec<Worker<J>>,
    /// Failed starts in a row, and when the first of them was.
    fails: u32,
    first_fail: Option<Instant>,
    /// No new readers before this.
    paused_until: Option<Instant>,
    error: Option<String>,
}

pub struct Workers<J, R> {
    group: Arc<ProcessGroup>,
    lanes: Vec<Lane<J, R>>,
    tx: Sender<Msg<R>>,
    rx: Receiver<Msg<R>>,
    next_worker: u64,
    next_job: u64,
    /// Events found outside `poll` (`submit` and `set_target` look at the channel too), handed out by the next `poll`.
    pending: Vec<(String, Event<J, R>)>,
    /// Readers told to stop, until they have gone.
    stopping: Vec<(Child, Instant)>,
    /// The warning about readers outside the job object is given once.
    warned: bool,
}

/// End a process and collect it, so no handle and no process is left behind.
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn exit_text(status: Option<ExitStatus>) -> String {
    match status.and_then(|s| s.code()) {
        // Crashes carry NTSTATUS values, which nobody recognises in decimal.
        Some(code) if (code as u32) >= 0x8000_0000 => format!("exit code {:#010X}", code as u32),
        Some(code) => format!("exit code {code}"),
        None => "no exit code".to_string(),
    }
}

/// The exit status of a reader whose output has ended. It has normally gone
/// already; one that lingers (still being torn down) is killed.
fn collect(child: &mut Child) -> Option<ExitStatus> {
    let until = Instant::now() + Duration::from_millis(300);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                return child.wait().ok();
            }
        }
    }
}

/// Where a reader's stderr goes: nowhere, or appended to the file named by
/// `SLEUTH_READER_STDERR` when someone is looking for a fault.
fn stderr_target() -> Stdio {
    match std::env::var_os("SLEUTH_READER_STDERR") {
        Some(path) if !path.is_empty() => std::fs::OpenOptions::new().create(true).append(true).open(path).map(Stdio::from).unwrap_or_else(|_| Stdio::null()),
        _ => Stdio::null(),
    }
}

/// The thread behind a reader's stdout: lines in, messages out.
fn read_replies<R: DeserializeOwned>(stdout: ChildStdout, worker: u64, tx: Sender<Msg<R>>) {
    let mut stdout = BufReader::with_capacity(64 * 1024, stdout);
    let mut line = Vec::new();
    let mut ready = false;
    let detail = loop {
        match wire::read_line(&mut stdout, &mut line) {
            Ok(true) => {}
            Ok(false) => break String::new(),
            Err(e) => break format!("its answer could not be read: {e}"),
        }
        if wire::is_blank(&line) {
            continue;
        }
        let msg = if !ready {
            match serde_json::from_slice::<wire::Ready>(&line) {
                Ok(r) if r.ready => {
                    ready = true;
                    Msg::Ready { worker }
                }
                _ => break "it did not answer as a reader when it started".to_string(),
            }
        } else if let Ok(no) = serde_json::from_slice::<wire::Refusal>(&line) {
            Msg::Refused { worker, id: no.id, error: no.error }
        } else {
            match serde_json::from_slice::<wire::ReplyIn<R>>(&line) {
                Ok(r) => Msg::Reply { worker, id: r.id, reply: r.reply, at: Instant::now() },
                Err(e) => break format!("its answer could not be understood: {e}"),
            }
        };
        if tx.send(msg).is_err() {
            return;
        }
        // One huge reading should not keep its buffer for the reader's whole life.
        if line.capacity() > (1 << 20) {
            line = Vec::new();
        }
    };
    let _ = tx.send(Msg::Closed { worker, detail });
}

/// Tells the owner when an in-process worker thread ends, however it ends.
struct Gone<R> {
    worker: u64,
    tx: Sender<Msg<R>>,
}

impl<R> Drop for Gone<R> {
    fn drop(&mut self) {
        let detail = if std::thread::panicking() { "the reader panicked".to_string() } else { String::new() };
        let _ = self.tx.send(Msg::Closed { worker: self.worker, detail });
    }
}

// Stopping and killing need nothing from the job and reply types, and `Drop` may not ask for more.
impl<J, R> Workers<J, R> {
    /// Ask an idle reader to stop: it sees the end of its stdin and exits.
    fn retire(stopping: &mut Vec<(Child, Instant)>, worker: Worker<J>, now: Instant) {
        if let Kind::Process { child, stdin } = worker.kind {
            drop(stdin);
            stopping.push((child, now));
        }
    }

    /// Forget readers that have gone; kill those that will not go.
    fn reap(&mut self, now: Instant, limit: Duration) {
        self.stopping.retain_mut(|(child, since)| match child.try_wait() {
            Ok(None) if now.saturating_duration_since(*since) < limit => true,
            Ok(None) => {
                kill(child);
                false
            }
            _ => false,
        });
    }

    fn kill_all(&mut self) {
        for lane in &mut self.lanes {
            for worker in lane.workers.drain(..) {
                if let Kind::Process { mut child, stdin } = worker.kind {
                    drop(stdin);
                    kill(&mut child);
                }
            }
        }
        for (mut child, _) in self.stopping.drain(..) {
            kill(&mut child);
        }
    }

    fn lane(&self, name: &str) -> Option<&Lane<J, R>> {
        self.lanes.iter().find(|l| l.name == name)
    }

    /// (workers alive, of which busy). Alive includes readers still starting,
    /// which take no job yet, so "alive minus busy" is not a promise that `submit` succeeds.
    pub fn counts(&self, lane: &str) -> (u32, u32) {
        self.lane(lane).map(|l| (l.workers.len() as u32, l.workers.iter().filter(|w| w.busy.is_some()).count() as u32)).unwrap_or((0, 0))
    }

    /// Why the lane has stopped starting readers (the program is missing, or
    /// it dies as soon as it starts), until one starts again.
    pub fn lane_error(&self, lane: &str) -> Option<String> {
        self.lane(lane).and_then(|l| l.error.clone())
    }

    /// Process ids of the lane's reader processes (none for an in-process lane).
    pub fn pids(&self, lane: &str) -> Vec<u32> {
        let Some(lane) = self.lane(lane) else { return Vec::new() };
        lane.workers
            .iter()
            .filter_map(|w| match &w.kind {
                Kind::Process { child, .. } => Some(child.id()),
                Kind::Thread { .. } => None,
            })
            .collect()
    }
}

impl<J, R> Drop for Workers<J, R> {
    /// The job object would end the readers when the last handle to it
    /// closes, but the group is shared and may outlive these lanes.
    fn drop(&mut self) {
        self.kill_all();
    }
}

impl<J: Serialize + Clone + Send + 'static, R: DeserializeOwned + Send + 'static> Workers<J, R> {
    pub fn new(group: Arc<ProcessGroup>) -> Self {
        let (tx, rx) = channel();
        Workers { group, lanes: Vec::new(), tx, rx, next_worker: 1, next_job: 1, pending: Vec::new(), stopping: Vec::new(), warned: false }
    }

    /// `init` is sent as the first line to every new worker of the lane (settings); it can be replaced later with `set_init` (new workers get the new one; running ones are restarted when idle).
    /// Adding a lane that exists replaces how its workers are started, in the same way.
    pub fn add_lane(&mut self, lane: &str, spawn: Spawn<J, R>, init: Value) {
        match self.lanes.iter_mut().find(|l| l.name == lane) {
            Some(l) => {
                l.spawn = spawn;
                l.init = Arc::new(init);
                l.init_gen += 1;
                (l.fails, l.first_fail, l.paused_until) = (0, None, None);
            }
            None => self.lanes.push(Lane { name: lane.to_string(), spawn, init: Arc::new(init), init_gen: 0, target: 0, workers: Vec::new(), fails: 0, first_fail: None, paused_until: None, error: None }),
        }
        self.settle(Instant::now());
    }

    pub fn set_init(&mut self, lane: &str, init: Value) {
        if let Some(l) = self.lanes.iter_mut().find(|l| l.name == lane) {
            l.init = Arc::new(init);
            l.init_gen += 1;
            // New settings may be what the readers needed to start.
            (l.fails, l.first_fail, l.paused_until) = (0, None, None);
        }
        self.settle(Instant::now());
    }

    /// Bring the lane to `n` workers: start more, or stop idle extras now and busy extras after their job. Never kills a job in progress.
    pub fn set_target(&mut self, lane: &str, n: u32) {
        if let Some(l) = self.lanes.iter_mut().find(|l| l.name == lane) {
            l.target = n;
        }
        self.settle(Instant::now());
    }

    /// Give a job to an idle worker of the lane. `Err(job)` hands it back when none is idle
    /// (or none is ready yet: a new reader takes jobs once it has started and read its settings).
    pub fn submit(&mut self, lane: &str, job: J, timeout: Duration) -> Result<(), J> {
        let now = Instant::now();
        // A reader that died since the last poll must not be given a job.
        self.settle(now);
        let Some(l) = self.lanes.iter_mut().find(|l| l.name == lane) else { return Err(job) };
        let Some(w) = l.workers.iter_mut().find(|w| w.ready && w.busy.is_none() && w.init_gen == l.init_gen) else { return Err(job) };
        let id = self.next_job;
        match &w.kind {
            Kind::Process { stdin, .. } => {
                let line = match wire::to_line(&wire::JobOut { id, job: &job }) {
                    Ok(line) => line,
                    Err(e) => {
                        // Not a job any reader could take: say so instead of offering it back for ever.
                        self.pending.push((l.name.clone(), Event::Crashed { job, detail: format!("the job could not be written for the reader: {e}") }));
                        return Ok(());
                    }
                };
                // Fails only when the child has gone; the next settle replaces it.
                if stdin.send(line).is_err() {
                    return Err(job);
                }
            }
            Kind::Thread { jobs } => {
                if jobs.send((id, job.clone())).is_err() {
                    return Err(job);
                }
            }
        }
        self.next_job += 1;
        w.given += 1;
        w.busy = Some(Busy { id, job, started: now, deadline: now.checked_add(timeout) });
        Ok(())
    }

    /// Jobs given out and not finished yet.
    pub fn in_flight(&self) -> Vec<(String, J)> {
        self.lanes.iter().flat_map(|l| l.workers.iter().filter_map(|w| w.busy.as_ref().map(|b| (l.name.clone(), b.job.clone())))).collect()
    }

    /// Wait up to `wait` for events; also checks timeouts and dead workers. Returns (lane, event) pairs, possibly empty.
    /// Returns early, with nothing, when a new worker has become ready for a job.
    pub fn poll(&mut self, wait: Duration) -> Vec<(String, Event<J, R>)> {
        let start = Instant::now();
        let mut woke = false;
        loop {
            let now = Instant::now();
            woke |= self.settle(now);
            let waited = now.saturating_duration_since(start);
            if !self.pending.is_empty() || woke || waited >= wait {
                break;
            }
            let next_deadline = self.lanes.iter().flat_map(|l| &l.workers).filter_map(|w| w.busy.as_ref()?.deadline).min();
            let mut nap = (wait - waited).min(HOUSEKEEPING);
            if let Some(d) = next_deadline {
                nap = nap.min(d.saturating_duration_since(now));
            }
            // A timeout here is simply the time to look at the clocks again.
            if let Ok(msg) = self.rx.recv_timeout(nap) {
                woke |= self.handle(msg, Instant::now());
            }
        }
        std::mem::take(&mut self.pending)
    }

    /// Stop everything: idle workers exit at once; busy ones are killed. Their jobs are returned.
    /// All targets are left at 0.
    pub fn shutdown(&mut self) -> Vec<(String, J)> {
        let now = Instant::now();
        let mut jobs = Vec::new();
        for lane in &mut self.lanes {
            lane.target = 0;
            for mut worker in lane.workers.drain(..) {
                match worker.busy.take() {
                    Some(busy) => {
                        jobs.push((lane.name.clone(), busy.job));
                        // An in-process job cannot be stopped; its thread ends after it.
                        if let Kind::Process { mut child, stdin } = worker.kind {
                            drop(stdin);
                            kill(&mut child);
                        }
                    }
                    None => Self::retire(&mut self.stopping, worker, now),
                }
            }
        }
        while !self.stopping.is_empty() {
            self.reap(Instant::now(), SHUTDOWN_LIMIT);
            if !self.stopping.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        jobs
    }

    /// Bring everything up to date without waiting: take in what the readers
    /// have said, end jobs past their timeout, stop readers that are not
    /// wanted and start those that are. True when a reader became ready.
    fn settle(&mut self, now: Instant) -> bool {
        let mut woke = false;
        while let Ok(msg) = self.rx.try_recv() {
            woke |= self.handle(msg, now);
        }
        self.check_clocks(now);
        self.balance(now);
        self.reap(now, STOP_LIMIT);
        woke
    }

    fn find(&self, worker: u64) -> Option<(usize, usize)> {
        self.lanes.iter().enumerate().find_map(|(li, l)| l.workers.iter().position(|w| w.id == worker).map(|wi| (li, wi)))
    }

    /// True when the message made a worker ready for a job.
    fn handle(&mut self, msg: Msg<R>, now: Instant) -> bool {
        match msg {
            Msg::Ready { worker } => {
                let Some((li, wi)) = self.find(worker) else { return false };
                let lane = &mut self.lanes[li];
                lane.workers[wi].ready = true;
                lane.error = None;
                true
            }
            Msg::Reply { worker, id, reply, at } => {
                let Some((li, wi)) = self.find(worker) else { return false };
                let lane = &mut self.lanes[li];
                match lane.workers[wi].busy.take_if(|b| b.id == id) {
                    Some(busy) => {
                        // A reader that works: earlier failed starts were something passing.
                        (lane.fails, lane.first_fail, lane.error) = (0, None, None);
                        let millis = at.saturating_duration_since(busy.started).as_millis() as u64;
                        self.pending.push((lane.name.clone(), Event::Done { job: busy.job, reply, millis }));
                    }
                    None => debug!(lane = %lane.name, id, "an answer nobody was waiting for"),
                }
                false
            }
            Msg::Refused { worker, id, error } => {
                let Some((li, wi)) = self.find(worker) else { return false };
                let lane = &mut self.lanes[li];
                if let Some(busy) = lane.workers[wi].busy.take_if(|b| b.id == id) {
                    self.pending.push((lane.name.clone(), Event::Crashed { job: busy.job, detail: format!("the reader gave no answer: {error}") }));
                }
                false
            }
            Msg::Closed { worker, detail } => {
                let Some((li, wi)) = self.find(worker) else { return false };
                let lane = &mut self.lanes[li];
                let w = lane.workers.remove(wi);
                let detail = match w.kind {
                    Kind::Process { mut child, stdin } => {
                        drop(stdin);
                        if detail.is_empty() {
                            format!("the reader process ended ({})", exit_text(collect(&mut child)))
                        } else {
                            kill(&mut child);
                            format!("the reader process was stopped: {detail}")
                        }
                    }
                    Kind::Thread { .. } if detail.is_empty() => "the reader stopped".to_string(),
                    Kind::Thread { .. } => detail,
                };
                match w.busy {
                    Some(busy) => self.pending.push((lane.name.clone(), Event::Crashed { job: busy.job, detail })),
                    // Never had a job: it is the reader that is broken, not a file.
                    None if w.given == 0 => Self::start_failed(lane, detail, now),
                    None => warn!(lane = %lane.name, "{detail} while idle; it will be replaced"),
                }
                false
            }
        }
    }

    /// Jobs past their timeout, and readers that never became ready.
    fn check_clocks(&mut self, now: Instant) {
        for lane in &mut self.lanes {
            let mut wi = 0;
            while wi < lane.workers.len() {
                let w = &lane.workers[wi];
                let timed_out = w.busy.as_ref().is_some_and(|b| b.deadline.is_some_and(|d| d <= now));
                let stuck = !w.ready && now.saturating_duration_since(w.started) >= START_LIMIT;
                if !timed_out && !stuck {
                    wi += 1;
                    continue;
                }
                let w = lane.workers.remove(wi);
                // An in-process job cannot be killed: its thread is left to
                // finish, and what it answers then is ignored.
                if let Kind::Process { mut child, stdin } = w.kind {
                    drop(stdin);
                    kill(&mut child);
                }
                match w.busy {
                    Some(busy) => {
                        let millis = now.saturating_duration_since(busy.started).as_millis() as u64;
                        self.pending.push((lane.name.clone(), Event::TimedOut { job: busy.job, millis }));
                    }
                    None => Self::start_failed(lane, format!("the reader process was not ready {} s after it started", START_LIMIT.as_secs()), now),
                }
            }
        }
    }

    fn start_failed(lane: &mut Lane<J, R>, detail: String, now: Instant) {
        match lane.first_fail {
            Some(first) if now.saturating_duration_since(first) <= START_WINDOW => lane.fails += 1,
            _ => (lane.fails, lane.first_fail) = (1, Some(now)),
        }
        warn!(lane = %lane.name, attempt = lane.fails, "a reader did not start: {detail}");
        if lane.fails >= START_FAILS {
            (lane.fails, lane.first_fail) = (0, None);
            lane.paused_until = Some(now + START_PAUSE);
            lane.error = Some(format!("The {} readers do not start: {detail}. Trying again every {} minutes.", lane.name, START_PAUSE.as_secs() / 60));
        }
    }

    /// Stop readers that are not wanted and start those that are missing.
    fn balance(&mut self, now: Instant) {
        for lane in &mut self.lanes {
            // Idle readers with old settings go first; `submit` would not use them anyway.
            let mut wi = 0;
            while wi < lane.workers.len() {
                let w = &lane.workers[wi];
                let over = lane.workers.len() > lane.target as usize;
                if w.busy.is_none() && (w.init_gen != lane.init_gen || (over && !w.ready)) {
                    Self::retire(&mut self.stopping, lane.workers.remove(wi), now);
                } else {
                    wi += 1;
                }
            }
            // Then idle extras. Busy extras stay until their job is done and are caught here afterwards.
            while lane.workers.len() > lane.target as usize {
                let Some(wi) = lane.workers.iter().rposition(|w| w.busy.is_none()) else { break };
                Self::retire(&mut self.stopping, lane.workers.remove(wi), now);
            }
            if lane.paused_until.is_some_and(|until| until <= now) {
                lane.paused_until = None;
            }
            while lane.workers.len() < lane.target as usize && lane.paused_until.is_none() {
                let id = self.next_worker;
                self.next_worker += 1;
                let started = match &lane.spawn {
                    Spawn::Process { exe, args, env } => Self::start_process((exe, args, env), &lane.init, id, self.tx.clone(), &self.group, &mut self.warned),
                    Spawn::InProcess(run) => Self::start_thread(run.clone(), lane.init.clone(), id, self.tx.clone()),
                };
                match started {
                    Ok(kind) => {
                        // A thread is ready as it is; a process says so when it has read its settings.
                        let ready = matches!(kind, Kind::Thread { .. });
                        lane.workers.push(Worker { id, kind, init_gen: lane.init_gen, started: now, ready, given: 0, busy: None });
                    }
                    Err(e) => {
                        Self::start_failed(lane, format!("{e:#}"), now);
                        // One attempt per round: a missing program is not hammered.
                        break;
                    }
                }
            }
        }
    }

    fn start_process(program: (&Path, &[String], &[(String, String)]), init: &Value, worker: u64, tx: Sender<Msg<R>>, group: &ProcessGroup, warned: &mut bool) -> Result<Kind<J>> {
        let (exe, args, env) = program;
        let first = wire::to_line(init).context("the settings could not be written for the reader")?;
        let mut cmd = Command::new(exe);
        cmd.args(args).envs(env.iter().map(|(k, v)| (k, v))).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(stderr_target());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows::Win32::System::Threading::{CREATE_NO_WINDOW, IDLE_PRIORITY_CLASS};
            cmd.creation_flags(CREATE_NO_WINDOW.0 | IDLE_PRIORITY_CLASS.0);
        }
        let mut child = cmd.spawn().with_context(|| format!("{} could not be started", exe.display()))?;
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            if let Err(e) = group.assign(child.as_raw_handle())
                && !*warned
            {
                *warned = true;
                warn!("reader processes run outside the job object ({e:#}): their CPU time is not counted as ours, and they are ended one by one");
            }
        }
        #[cfg(not(windows))]
        let _ = (group, warned);
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            kill(&mut child);
            anyhow::bail!("{} started without its pipes", exe.display());
        };
        let (lines, to_write) = channel::<Vec<u8>>();
        let _ = lines.send(first);
        // The thread ends, and closes the child's stdin, when `lines` is dropped or the child has gone.
        let writer = std::thread::Builder::new().name("sleuth-reader-stdin".into()).spawn(move || {
            for line in to_write {
                if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let reader = std::thread::Builder::new().name("sleuth-reader-stdout".into()).spawn(move || read_replies(stdout, worker, tx));
        if let Err(e) = writer.and(reader) {
            kill(&mut child);
            return Err(e).context("no thread could be started for the reader");
        }
        Ok(Kind::Process { child, stdin: lines })
    }

    fn start_thread(run: Arc<Handler<J, R>>, init: Arc<Value>, worker: u64, tx: Sender<Msg<R>>) -> Result<Kind<J>> {
        let (jobs, to_run) = channel::<(u64, J)>();
        std::thread::Builder::new()
            .name("sleuth-lane-worker".into())
            .spawn(move || {
                let gone = Gone { worker, tx };
                for (id, job) in to_run {
                    let reply = run(&init, job);
                    if gone.tx.send(Msg::Reply { worker, id, reply, at: Instant::now() }).is_err() {
                        break;
                    }
                }
            })
            .context("no thread could be started for the reader")?;
        Ok(Kind::Thread { jobs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    struct Job {
        n: u64,
        sleep_ms: u64,
    }

    fn job(n: u64) -> Job {
        Job { n, sleep_ms: 0 }
    }

    /// Answers `n * init.times`, after `sleep_ms`; `n == 13` panics.
    fn lanes(init: Value) -> Workers<Job, u64> {
        let mut w = Workers::new(Arc::new(ProcessGroup::new().unwrap()));
        let run = |init: &Value, job: Job| -> u64 {
            std::thread::sleep(Duration::from_millis(job.sleep_ms));
            assert!(job.n != 13, "unlucky");
            job.n * init["times"].as_u64().unwrap_or(1)
        };
        w.add_lane("fast", Spawn::InProcess(Arc::new(run)), init);
        w
    }

    /// Poll until `n` events have come, or 10 s have passed.
    fn events(w: &mut Workers<Job, u64>, n: usize) -> Vec<(String, Event<Job, u64>)> {
        let until = Instant::now() + Duration::from_secs(10);
        let mut all = Vec::new();
        while all.len() < n && Instant::now() < until {
            all.extend(w.poll(Duration::from_millis(100)));
        }
        all
    }

    #[test]
    fn jobs_go_round_and_every_one_comes_back() {
        let mut w = lanes(json!({ "times": 3 }));
        w.set_target("fast", 3);
        assert_eq!(w.counts("fast"), (3, 0));
        let mut todo: Vec<Job> = (21..=60).map(job).collect();
        let mut done = Vec::new();
        while done.len() < 40 {
            while let Some(j) = todo.pop() {
                if let Err(back) = w.submit("fast", j, Duration::from_secs(10)) {
                    todo.push(back);
                    break;
                }
            }
            assert!(w.counts("fast").1 <= 3);
            for (lane, e) in w.poll(Duration::from_millis(200)) {
                assert_eq!(lane, "fast");
                match e {
                    Event::Done { job, reply, .. } => done.push((job.n, reply)),
                    _ => panic!("a job failed"),
                }
            }
        }
        done.sort();
        assert_eq!(done, (21..=60).map(|n| (n, n * 3)).collect::<Vec<_>>());
        assert_eq!(w.counts("fast"), (3, 0));
        assert!(w.in_flight().is_empty());
    }

    #[test]
    fn submit_hands_the_job_back_when_nobody_is_idle() {
        let mut w = lanes(json!({}));
        assert_eq!(w.submit("fast", job(1), Duration::from_secs(5)), Err(job(1)), "no workers yet");
        assert_eq!(w.submit("nowhere", job(1), Duration::from_secs(5)), Err(job(1)), "no such lane");
        assert_eq!(w.counts("nowhere"), (0, 0));
        w.set_target("fast", 1);
        let slow = Job { n: 2, sleep_ms: 150 };
        assert_eq!(w.submit("fast", slow.clone(), Duration::from_secs(5)), Ok(()));
        assert_eq!(w.submit("fast", job(3), Duration::from_secs(5)), Err(job(3)));
        assert_eq!(w.in_flight(), [("fast".to_string(), slow)]);
        assert_eq!(w.counts("fast"), (1, 1));
        let got = events(&mut w, 1);
        assert!(matches!(&got[0].1, Event::Done { reply: 2, millis, .. } if *millis >= 140));
        assert_eq!(w.submit("fast", job(3), Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn the_target_goes_up_and_down_without_killing_a_job() {
        let mut w = lanes(json!({}));
        w.set_target("fast", 4);
        assert_eq!(w.counts("fast"), (4, 0));
        for n in 1..=2 {
            assert!(w.submit("fast", Job { n, sleep_ms: 200 }, Duration::from_secs(5)).is_ok());
        }
        // Down to one: the two idle ones go now, one busy one after its job.
        w.set_target("fast", 1);
        assert_eq!(w.counts("fast"), (2, 2));
        let got = events(&mut w, 2);
        assert_eq!(got.iter().filter(|(_, e)| matches!(e, Event::Done { .. })).count(), 2, "both jobs finished");
        assert_eq!(w.counts("fast"), (1, 0));
        w.set_target("fast", 0);
        assert_eq!(w.counts("fast"), (0, 0));
        assert_eq!(w.submit("fast", job(9), Duration::from_secs(5)), Err(job(9)));
        w.set_target("fast", 2);
        assert_eq!(w.counts("fast"), (2, 0));
        assert_eq!(w.submit("fast", job(9), Duration::from_secs(5)), Ok(()));
        assert!(matches!(events(&mut w, 1)[0].1, Event::Done { reply: 9, .. }));
    }

    #[test]
    fn a_job_past_its_timeout_is_reported_and_its_worker_replaced() {
        let mut w = lanes(json!({}));
        w.set_target("fast", 1);
        let slow = Job { n: 5, sleep_ms: 1_500 };
        assert!(w.submit("fast", slow.clone(), Duration::from_millis(100)).is_ok());
        let asked = Instant::now();
        let got = w.poll(Duration::from_secs(5));
        assert!(asked.elapsed() < Duration::from_millis(1_000), "poll wakes for the timeout, not for its own wait");
        assert_eq!(got.len(), 1);
        match &got[0].1 {
            Event::TimedOut { job, millis } => {
                assert_eq!(job, &slow);
                assert!((100..1_000).contains(millis), "{millis}");
            }
            _ => panic!("expected a timeout"),
        }
        assert_eq!(w.counts("fast"), (1, 0), "a new worker took its place");
        assert!(w.submit("fast", job(6), Duration::from_secs(5)).is_ok());
        let got = events(&mut w, 1);
        assert!(matches!(got[0].1, Event::Done { reply: 6, .. }));
        // The abandoned thread finishes later; its answer must not turn up as an event.
        assert!(w.poll(Duration::from_millis(1_700)).is_empty());
    }

    #[test]
    fn a_panic_is_a_crash_of_that_job_only() {
        let mut w = lanes(json!({}));
        w.set_target("fast", 2);
        assert!(w.submit("fast", job(13), Duration::from_secs(5)).is_ok());
        assert!(w.submit("fast", job(4), Duration::from_secs(5)).is_ok());
        let got = events(&mut w, 2);
        assert!(got.iter().any(|(_, e)| matches!(e, Event::Crashed { job, detail } if job.n == 13 && detail == "the reader panicked")));
        assert!(got.iter().any(|(_, e)| matches!(e, Event::Done { reply: 4, .. })));
        assert_eq!(w.counts("fast"), (2, 0), "replaced");
        assert_eq!(w.lane_error("fast"), None);
    }

    #[test]
    fn new_settings_reach_the_next_job() {
        let mut w = lanes(json!({ "times": 2 }));
        w.set_target("fast", 2);
        assert!(w.submit("fast", Job { n: 7, sleep_ms: 100 }, Duration::from_secs(5)).is_ok());
        w.set_init("fast", json!({ "times": 10 }));
        // The job in progress keeps the settings it started with.
        assert_eq!(w.counts("fast"), (2, 1));
        assert!(w.submit("fast", job(7), Duration::from_secs(5)).is_ok());
        let mut replies: Vec<u64> = events(&mut w, 2).into_iter().map(|(_, e)| if let Event::Done { reply, .. } = e { reply } else { 0 }).collect();
        replies.sort();
        assert_eq!(replies, [14, 70]);
        assert_eq!(w.counts("fast"), (2, 0));
        assert!(w.submit("fast", job(1), Duration::from_secs(5)).is_ok());
        assert!(w.submit("fast", job(1), Duration::from_secs(5)).is_ok());
        assert!(events(&mut w, 2).iter().all(|(_, e)| matches!(e, Event::Done { reply: 10, .. })));
    }

    #[test]
    fn shutdown_returns_the_jobs_in_progress() {
        let mut w = lanes(json!({}));
        w.set_target("fast", 3);
        let slow = Job { n: 1, sleep_ms: 300 };
        assert!(w.submit("fast", slow.clone(), Duration::from_secs(5)).is_ok());
        assert_eq!(w.shutdown(), [("fast".to_string(), slow)]);
        assert_eq!(w.counts("fast"), (0, 0));
        assert!(w.in_flight().is_empty());
        assert!(w.poll(Duration::from_millis(400)).is_empty(), "nothing restarts, and the late answer is dropped");
        assert_eq!(w.counts("fast"), (0, 0));
    }

    #[test]
    fn a_reader_program_that_is_missing_becomes_a_lane_error() {
        let mut w: Workers<Job, u64> = Workers::new(Arc::new(ProcessGroup::new().unwrap()));
        let exe = Path::new(env!("CARGO_MANIFEST_DIR")).join("no such folder").join("no-such-reader.exe");
        w.add_lane("ocr", Spawn::Process { exe, args: Vec::new(), env: Vec::new() }, json!({}));
        w.set_target("ocr", 2);
        assert_eq!(w.lane_error("ocr"), None, "one failure is not yet a verdict");
        for _ in 0..10 {
            assert!(w.poll(Duration::from_millis(1)).is_empty());
        }
        let error = w.lane_error("ocr").expect("five failed starts");
        assert!(error.starts_with("The ocr readers do not start: ") && error.contains("no-such-reader.exe"), "{error}");
        assert_eq!(w.counts("ocr"), (0, 0));
        assert_eq!(w.submit("ocr", job(1), Duration::from_secs(1)), Err(job(1)));
        // It has stopped trying: no sixth attempt would change the wording, but the pause must be on.
        assert!(w.lanes[0].paused_until.is_some_and(|t| t > Instant::now() + Duration::from_secs(200)));
        // After the pause it tries again.
        w.lanes[0].paused_until = Some(Instant::now());
        w.poll(Duration::from_millis(1));
        assert!(w.lanes[0].fails >= 1 && w.lanes[0].paused_until.is_none());
    }
}
