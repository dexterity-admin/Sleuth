//! One contender for a lease, to prove with real processes that two never
//! hold it together: `lease_race <dir> <name> <rounds> <id>`.
//!
//! Each round it takes the lease `<name>` in `<dir>`, writes `enter <id>`
//! to `<dir>\<name>.log`, keeps the lease 20 ms, writes `leave <id>` and
//! releases it. Start several at once; if the lease works, the log is
//! strictly `enter x`, `leave x`, `enter y`, `leave y`, ...

use sleuth_office::lease::{Acquire, Identity, Leases};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(dir), Some(name), Some(rounds), Some(id)) = (args.get(1), args.get(2), args.get(3).and_then(|r| r.parse::<u32>().ok()), args.get(4)) else {
        eprintln!("usage: lease_race <dir> <name> <rounds> <id>");
        std::process::exit(2);
    };
    let dir = PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("{id}: {}: {e}", dir.display());
        std::process::exit(1);
    }
    // Every contender is this user on this PC, as several readers are: only the process differs.
    let leases = Leases::new(&dir, Identity::current());
    let mut log = match std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{name}.log"))) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{id}: log: {e}");
            std::process::exit(1);
        }
    };
    let mut say = |what: &str| {
        if let Err(e) = log.write_all(format!("{what} {id}\n").as_bytes()) {
            eprintln!("{id}: log: {e}");
            std::process::exit(1);
        }
    };
    let (mut waits, mut errors) = (0u32, 0u32);
    for round in 0..rounds {
        loop {
            match leases.acquire(name, Duration::from_secs(60)) {
                Acquire::Mine(lease) => {
                    say("enter");
                    std::thread::sleep(Duration::from_millis(20));
                    say("leave");
                    lease.release();
                    break;
                }
                Acquire::HeldBy(_) => {
                    waits += 1;
                    std::thread::sleep(Duration::from_millis(1 + (round as u64 + waits as u64) % 4));
                }
                Acquire::Unavailable(e) => {
                    errors += 1;
                    eprintln!("{id}: unavailable: {e}");
                    if errors > 500 {
                        std::process::exit(1);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
    println!("{id}: {rounds} rounds, found it held {waits} times, unavailable {errors} times");
}
