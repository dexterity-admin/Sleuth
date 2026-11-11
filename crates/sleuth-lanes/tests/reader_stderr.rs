//! `SLEUTH_READER_STDERR`: where a reader's standard error goes when someone
//! is looking for a fault. Alone in this file, because the variable belongs
//! to the whole test process.
#![cfg(windows)]

mod common;

use common::{Job, Rig};
use serde_json::json;
use std::time::Duration;

#[test]
fn stderr_is_appended_to_the_named_file() {
    let Some(exe) = common::child_exe() else { return };
    // Beside the built example: wherever the target folder is, it can be written.
    let log = exe.with_file_name(format!("reader-stderr-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    // No other thread exists yet in this test process.
    unsafe { std::env::set_var("SLEUTH_READER_STDERR", &log) };
    let Some(mut rig) = Rig::new(1, json!({}), &[]) else { return };
    let long = Duration::from_secs(600);
    // What the handler prints to standard output is led to standard error...
    rig.give(Job::op("noisy"), long);
    assert_eq!(rig.done().text, "quiet:0");
    // ...and so is the message of the panic that ends the reader.
    rig.give(Job::op("crash"), long);
    assert_eq!(rig.crash().0, "crash");
    // A second process appends; it does not start the file again.
    rig.give(Job::op("noisy"), long);
    assert_eq!(rig.done().text, "quiet:0");
    drop(rig);
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    assert!(text.contains("this file crashes its reader"), "the panic message is missing:\n{text}");
    assert_eq!(text.matches("not a real answer").count(), 2, "Rust's standard output, twice:\n{text}");
    assert_eq!(text.matches("half a line, no line break").count(), 2);
    assert_eq!(text.matches("printed by C code").count(), 2, "the C runtime's standard output, twice:\n{text}");
}
