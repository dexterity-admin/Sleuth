//! Four processes contend for one lease (the `lease_race` example); the log
//! they share must never show two inside at once.
//!
//! The folder is `SLEUTH_TEST_DIR` when set, else the temporary folder; set
//! it to a folder on the share to test the share. `SLEUTH_RACE_ROUNDS`
//! changes the rounds per process (default 25).

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The example, built next to this test by `cargo test`.
fn contender() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.parent()?.join("examples").join(format!("lease_race{}", std::env::consts::EXE_SUFFIX));
    p.exists().then_some(p)
}

#[test]
fn four_processes_never_hold_the_lease_together() {
    let Some(exe) = contender() else {
        eprintln!("the lease_race example is not built: skipped");
        return;
    };
    let root = std::env::var_os("SLEUTH_TEST_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = root.join(format!("sl-office-lease-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rounds: usize = std::env::var("SLEUTH_RACE_ROUNDS").ok().and_then(|r| r.parse().ok()).unwrap_or(25);
    let t0 = std::time::Instant::now();
    let children: Vec<_> = (0..4)
        .map(|id| Command::new(&exe).arg(&dir).arg("race").arg(rounds.to_string()).arg(format!("p{id}")).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("start a contender"))
        .collect();
    let mut unavailable = 0;
    for child in children {
        let out = child.wait_with_output().expect("a contender's result");
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        print!("{stdout}");
        unavailable += stderr.lines().filter(|l| l.contains("unavailable")).count();
        assert!(out.status.success(), "a contender failed: {stderr}");
    }
    let log = std::fs::read_to_string(dir.join("race.log")).expect("the log");
    let lines: Vec<&str> = log.lines().collect();
    let mut inside: Option<&str> = None;
    for (i, line) in lines.iter().enumerate() {
        match (line.split_once(' '), inside) {
            (Some(("enter", id)), None) => inside = Some(id),
            (Some(("leave", id)), Some(holder)) if id == holder => inside = None,
            _ => panic!("line {} of the log is {line:?} while {inside:?} holds the lease:\n{log}", i + 1),
        }
    }
    assert!(inside.is_none(), "the log ends with {inside:?} inside");
    assert_eq!(lines.len(), 4 * rounds * 2, "every round of every process entered and left once");
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n != "race.log").collect();
    println!("lease race in {}: 4 processes x {rounds} rounds, {} entries in order, {unavailable} unavailable answers, {:.1} s, left behind: {left:?}", dir.display(), lines.len() / 2, t0.elapsed().as_secs_f64());
    let _ = std::fs::remove_dir_all(dir);
}
