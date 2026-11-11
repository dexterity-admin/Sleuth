//! Lanes of real reader processes: `examples/echo_child.rs`, started as the
//! engine starts its readers. Build it first:
//! `cargo build -p sleuth-lanes --examples`. Without it every test here says
//! it was skipped and passes.
#![cfg(windows)]

mod common;

use common::{Job, Rig, alive, all_gone, priority_class};
use serde_json::json;
use sleuth_lanes::Event;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// Long enough for a reader at idle priority on a PC whose cores are all taken.
const LONG: Duration = Duration::from_secs(600);

#[test]
fn echo_200_jobs_across_4_workers() {
    let Some(mut rig) = Rig::new(4, json!({ "v": 1 }), &[]) else { return };
    rig.warm(4);
    assert_eq!(rig.w.counts("lane"), (4, 0));
    let started: BTreeSet<u32> = rig.w.pids("lane").into_iter().collect();
    assert_eq!(started.len(), 4);
    let mut todo: Vec<Job> = (0..200).map(|n| Job::echo(&format!("job {n} \u{2713} \"quoted\"\nsecond line"))).collect();
    let mut answers = Vec::new();
    let until = Instant::now() + LONG;
    while answers.len() < 200 {
        assert!(Instant::now() < until, "only {} of 200 came back", answers.len());
        while let Some(job) = todo.pop() {
            if let Err(back) = rig.w.submit("lane", job, LONG) {
                todo.push(back);
                break;
            }
        }
        for (_, event) in rig.w.poll(Duration::from_millis(100)) {
            match event {
                Event::Done { job, reply, .. } => {
                    assert_eq!(reply.text, job.text);
                    assert_eq!(reply.init, json!({ "v": 1 }));
                    answers.push((job.text, reply.pid));
                }
                _ => panic!("an echo failed"),
            }
        }
    }
    let texts: BTreeSet<&str> = answers.iter().map(|a| a.0.as_str()).collect();
    assert_eq!(texts.len(), 200, "every job answered once");
    let used: BTreeSet<u32> = answers.iter().map(|a| a.1).collect();
    assert_eq!(used, started, "all four workers took part, and no other process");
    assert_eq!(rig.w.counts("lane"), (4, 0));
    assert!(rig.w.in_flight().is_empty());
}

#[test]
fn a_crashing_job_is_reported_and_the_lane_recovers() {
    let Some(mut rig) = Rig::new(1, json!({}), &[]) else { return };
    rig.warm(1);
    let first = rig.w.pids("lane")[0];
    // A panic in the handler: `serve` ends the process.
    rig.give(Job::op("crash"), LONG);
    assert_eq!(rig.crash(), ("crash".to_string(), "the reader process ended (exit code 101)".to_string()));
    assert!(!alive(first));
    // A crash in native code: no error reporter in between, the exception code comes straight back.
    rig.give(Job::op("segv"), LONG);
    assert_eq!(rig.crash(), ("segv".to_string(), "the reader process ended (exit code 0xC0000005)".to_string()));
    rig.give(Job::echo("after the crash"), LONG);
    let reply = rig.done();
    assert_eq!(reply.text, "after the crash");
    assert_ne!(reply.pid, first, "a new process");
    // A reader that simply exits in the middle of a job.
    rig.give(Job::op("exit"), LONG);
    assert_eq!(rig.crash(), ("exit".to_string(), "the reader process ended (exit code 7)".to_string()));
    rig.give(Job::echo("and again"), LONG);
    assert_eq!(rig.done().text, "and again");
    assert_eq!(rig.w.counts("lane"), (1, 0));
    assert_eq!(rig.w.lane_error("lane"), None, "crashes with a job in hand are the file's fault, not the lane's");
}

#[test]
fn a_job_past_its_timeout_has_its_worker_killed() {
    let Some(mut rig) = Rig::new(1, json!({}), &[]) else { return };
    rig.warm(1);
    let first = rig.w.pids("lane")[0];
    rig.give(Job::sleep(120_000), Duration::from_millis(300));
    assert_eq!(rig.w.counts("lane"), (1, 1));
    assert_eq!(rig.w.in_flight(), [("lane".to_string(), Job::sleep(120_000))]);
    match rig.event() {
        Event::TimedOut { job, millis } => {
            assert_eq!(job, Job::sleep(120_000));
            assert!((300..10_000).contains(&millis), "{millis}");
        }
        _ => panic!("expected a timeout"),
    }
    assert!(!alive(first), "the stuck reader was killed");
    assert!(rig.w.in_flight().is_empty());
    rig.give(Job::echo("after the timeout"), LONG);
    let reply = rig.done();
    assert_eq!(reply.text, "after the timeout");
    assert_ne!(reply.pid, first);
}

#[test]
fn a_20_mb_reply_and_a_4_mb_job_survive() {
    let Some(mut rig) = Rig::new(1, json!({}), &[]) else { return };
    rig.give(Job { op: "big".into(), mb: 20, ..Default::default() }, LONG);
    let reply = rig.done();
    assert_eq!(reply.text.len(), 20 << 20);
    assert!(reply.text.bytes().all(|b| b == b'x'));
    // The other way: a large job, with characters JSON has to escape.
    let text = "line \"one\"\n\ttab \u{00e9}\u{4e2d}\\ ".repeat((4 << 20) / 24);
    rig.give(Job::echo(&text), LONG);
    let back = rig.done();
    assert_eq!(back.text.len(), text.len());
    assert!(back.text == text);
    assert_eq!(back.pid, reply.pid, "the same reader, still in step");
    assert!(rig.group.peak_process_mb() >= 20, "{} MB", rig.group.peak_process_mb());
}

#[test]
fn children_run_at_idle_priority_inside_the_job() {
    const IDLE_PRIORITY_CLASS: u32 = 0x40;
    let Some(mut rig) = Rig::new(2, json!({}), &[]) else { return };
    rig.warm(2);
    let pids = rig.w.pids("lane");
    assert_eq!(pids.len(), 2);
    for pid in &pids {
        assert_eq!(priority_class(*pid), IDLE_PRIORITY_CLASS, "process {pid}");
    }
    assert!(rig.group.working_set_mb() >= 1, "two live readers hold memory");
    let before = rig.group.cpu_100ns();
    for _ in 0..2 {
        rig.give(Job { op: "burn".into(), ms: 1_000, ..Default::default() }, LONG);
    }
    rig.done();
    rig.done();
    let burnt = rig.group.cpu_100ns() - before;
    // Two readers spinning for a second each. At idle priority on a loaded
    // PC they get less than that, but never nothing.
    assert!(burnt >= 500_000, "the job object saw only {burnt} x 100 ns of CPU time");
    // Idle priority is still what they run at after working.
    for pid in &pids {
        assert_eq!(priority_class(*pid), IDLE_PRIORITY_CLASS);
    }
}

#[test]
fn dropping_the_workers_leaves_no_child_alive() {
    let Some(mut rig) = Rig::new(4, json!({}), &[]) else { return };
    rig.warm(4);
    let pids = rig.w.pids("lane");
    assert_eq!(pids.len(), 4);
    assert!(pids.iter().all(|p| alive(*p)));
    // One told to stop a moment ago, one in the middle of a job, two idle.
    rig.give(Job::sleep(120_000), LONG);
    rig.w.set_target("lane", 3);
    assert_eq!(rig.w.counts("lane"), (3, 1));
    let Rig { w, group, .. } = rig;
    drop(w);
    // Straight away, and with the job object still open: the workers do the killing.
    assert!(pids.iter().all(|p| !alive(*p)), "a reader outlived its workers");
    drop(group);
}

#[test]
fn lowering_the_target_lets_idle_children_exit_by_themselves() {
    let Some(mut rig) = Rig::new(3, json!({}), &[]) else { return };
    rig.warm(3);
    let before = rig.w.pids("lane");
    rig.give(Job::sleep(600), LONG);
    rig.w.set_target("lane", 0);
    assert_eq!(rig.w.counts("lane"), (1, 1), "the busy one finishes its job");
    assert_eq!(rig.done().text, "slept");
    assert_eq!(rig.w.counts("lane"), (0, 0));
    assert!(all_gone(&before), "stdin closed, so they exit");
    assert!(rig.w.poll(Duration::from_millis(50)).is_empty());
}

#[test]
fn shutdown_returns_the_job_in_progress_and_ends_every_child() {
    let Some(mut rig) = Rig::new(3, json!({}), &[]) else { return };
    rig.warm(3);
    let pids = rig.w.pids("lane");
    rig.give(Job::sleep(120_000), LONG);
    let back = rig.w.shutdown();
    assert_eq!(back, [("lane".to_string(), Job::sleep(120_000))]);
    assert!(pids.iter().all(|p| !alive(*p)));
    assert_eq!(rig.w.counts("lane"), (0, 0));
    assert!(rig.w.poll(Duration::from_millis(200)).is_empty());
    assert_eq!(rig.w.counts("lane"), (0, 0), "nothing starts again by itself");
}

#[test]
fn new_settings_restart_the_idle_children() {
    let Some(mut rig) = Rig::new(2, json!({ "v": 1 }), &[]) else { return };
    rig.warm(2);
    let old = rig.w.pids("lane");
    rig.give(Job::echo("a"), LONG);
    assert_eq!(rig.done().init, json!({ "v": 1 }));
    rig.w.set_init("lane", json!({ "v": 2, "why": "a setting changed" }));
    for _ in 0..4 {
        rig.give(Job::echo("b"), LONG);
        let reply = rig.done();
        assert_eq!(reply.init["v"], 2);
        assert!(!old.contains(&reply.pid));
    }
    assert_eq!(rig.w.counts("lane"), (2, 0));
    assert!(all_gone(&old));
}

#[test]
fn printing_in_the_handler_does_not_break_the_replies() {
    let Some(mut rig) = Rig::new(1, json!({}), &[]) else { return };
    // Rust and C code print to standard output, and a program started by the
    // handler reads its standard input: none of it reaches the two pipes.
    rig.give(Job::op("noisy"), LONG);
    let noisy = rig.done();
    assert_eq!(noisy.text, "quiet:0");
    for n in 0..20 {
        rig.give(Job::echo(&format!("still in step {n}")), LONG);
        let reply = rig.done();
        assert_eq!((reply.text, reply.pid), (format!("still in step {n}"), noisy.pid));
    }
    assert!(rig.seen.is_empty());
}

#[test]
fn a_reader_that_dies_at_startup_becomes_a_lane_error() {
    let Some(mut rig) = Rig::new(2, json!({}), &["die"]) else { return };
    let until = Instant::now() + LONG;
    while rig.w.lane_error("lane").is_none() {
        assert!(Instant::now() < until, "no error was reported");
        assert!(rig.w.submit("lane", Job::echo("x"), LONG).is_err(), "a reader that never started was given a job");
        rig.pump(Duration::from_millis(20));
    }
    let error = rig.w.lane_error("lane").unwrap_or_default();
    assert_eq!(error, "The lane readers do not start: the reader process ended (exit code 3). Trying again every 5 minutes.");
    assert!(rig.seen.is_empty(), "no file was blamed");
    // It has stopped trying: the reader started together with the fifth
    // failure goes too, and then nothing more is started.
    while rig.w.counts("lane") != (0, 0) {
        assert!(Instant::now() < until, "a reader was still being started");
        rig.pump(Duration::from_millis(20));
    }
    rig.pump(Duration::from_millis(500));
    assert_eq!(rig.w.counts("lane"), (0, 0));
    assert!(rig.w.lane_error("lane").is_some(), "the error stays until a reader starts");
    assert!(rig.seen.is_empty());
}
