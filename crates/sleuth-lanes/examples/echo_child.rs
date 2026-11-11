//! A reader process for the tests: it answers what it is asked.
//!
//! Started with the argument `die` it exits at once, as a reader program
//! that is broken would.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Deserialize)]
struct Job {
    /// "echo", "sleep", "crash", "segv", "exit", "big", "burn" or "noisy".
    op: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    ms: u64,
    #[serde(default)]
    mb: usize,
}

#[derive(Serialize)]
struct Reply {
    text: String,
    pid: u32,
    /// The settings this process was started with.
    init: Value,
}

unsafe extern "C" {
    fn puts(s: *const std::ffi::c_char) -> i32;
    fn fflush(stream: *mut std::ffi::c_void) -> i32;
}

fn answer(job: Job) -> String {
    match job.op.as_str() {
        "echo" => job.text,
        "sleep" => {
            std::thread::sleep(Duration::from_millis(job.ms));
            "slept".to_string()
        }
        "crash" => panic!("this file crashes its reader"),
        // What a native library does on a damaged file: write where it may not.
        "segv" => {
            unsafe { std::ptr::write_volatile(std::hint::black_box(0x10 as *mut u8), 1) };
            "survived".to_string()
        }
        "exit" => std::process::exit(7),
        "big" => "x".repeat(job.mb << 20),
        "burn" => {
            let until = Instant::now() + Duration::from_millis(job.ms);
            let mut x = 1u64;
            while Instant::now() < until {
                for i in 0..100_000u64 {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
                }
            }
            std::hint::black_box(x).to_string()
        }
        // What a careless handler or a C library does: write to standard output.
        "noisy" => {
            println!("{{\"id\":1,\"reply\":\"not a real answer\"}}");
            print!("half a line, no line break");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            unsafe {
                puts(c"printed by C code".as_ptr());
                fflush(std::ptr::null_mut());
            }
            // And a program started from here must not find the job pipe on its standard input.
            // (`findstr` reads its standard input to the end; given the pipe it would wait for ever.)
            let found = std::process::Command::new("findstr").arg("id").stdin(std::process::Stdio::inherit()).output().map(|o| o.stdout.len()).unwrap_or(0);
            format!("quiet:{found}")
        }
        other => format!("unknown op {other}"),
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("die") {
        std::process::exit(3);
    }
    let served = sleuth_lanes::serve(|init: &Value, job: Job| Reply { text: answer(job), pid: std::process::id(), init: init.clone() });
    if let Err(e) = served {
        eprintln!("echo_child: {e:#}");
        std::process::exit(1);
    }
}
