//! The reader process's side: jobs in on stdin, answers out on stdout.

use crate::wire;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs::File;
use std::io::{BufReader, Write};

/// The exit code of a reader whose handler panicked (what a Rust program that panics exits with).
const PANICKED: u32 = 101;

/// Run as a reader process: read the init line, then one job per line from stdin, answer each on stdout, return when stdin closes.
///
/// Sets this process to idle CPU priority first (only the CPU priority:
/// background mode would also lower the memory priority, and a large OCR
/// model then gets paged out between pages).
///
/// The two pipes are taken away from the rest of the process: after this,
/// its standard output leads to standard error and its standard input is
/// empty, so nothing the handler or a library prints can land between two
/// replies, and a program the handler starts cannot swallow the next job.
///
/// If the handler panics, or native code crashes, the process ends at once
/// (exit code 101, or the exception code) and the worker reports the job as
/// having crashed its reader. It ends by terminating itself rather than
/// through `abort`: an aborted process is first handed to Windows Error
/// Reporting, which runs at this process's idle priority, and on a busy PC
/// the worker would learn of the crash minutes later.
pub fn serve<I: DeserializeOwned, J: DeserializeOwned, R: Serialize>(mut handle: impl FnMut(&I, J) -> R) -> Result<()> {
    be_a_quiet_neighbour();
    let (input, mut output) = take_pipes()?;
    let mut input = BufReader::with_capacity(64 * 1024, input);
    let mut line = Vec::new();
    if !wire::read_line(&mut input, &mut line).context("reading the settings line")? {
        bail!("the worker closed the pipe before sending the settings");
    }
    let init: I = serde_json::from_slice(&line).context("the settings line could not be understood")?;
    output.write_all(&wire::to_line(&wire::Ready { ready: true })?)?;
    loop {
        if !wire::read_line(&mut input, &mut line).context("reading a job")? {
            return Ok(());
        }
        if wire::is_blank(&line) {
            continue;
        }
        let (id, job) = match serde_json::from_slice::<wire::JobIn<J>>(&line) {
            Ok(j) => (j.id, j.job),
            Err(e) => {
                // Answer if the line at least says which job it was; the worker
                // then knows without waiting for the timeout.
                let Ok(which) = serde_json::from_slice::<wire::IdOnly>(&line) else {
                    return Err(e).context("a job line could not be understood");
                };
                output.write_all(&wire::to_line(&wire::Refusal { id: which.id, error: format!("the job could not be understood: {e}") })?)?;
                continue;
            }
        };
        if line.capacity() > (1 << 20) {
            line = Vec::new();
        }
        let reply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&init, job))) {
            Ok(r) => r,
            // The panic message has gone to standard error already.
            Err(_) => die(PANICKED),
        };
        let bytes = match wire::to_line(&wire::ReplyOut { id, reply: &reply }) {
            Ok(b) => b,
            Err(e) => wire::to_line(&wire::Refusal { id, error: format!("the answer could not be written: {e}") })?,
        };
        drop(reply);
        output.write_all(&bytes)?;
    }
}

/// End this process now: no unwinding, no exit handlers, no error reporter.
fn die(code: u32) -> ! {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        let _ = TerminateProcess(GetCurrentProcess(), code);
    }
    let _ = code;
    std::process::abort()
}

/// The few kernel32 calls outside the workspace's `windows` features. They
/// are declared here so that this crate does not add features, which would
/// rebuild `windows` for every program in the workspace.
#[cfg(windows)]
mod kernel32 {
    use core::ffi::c_void;

    pub const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    pub const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    pub const SEM_FAILCRITICALERRORS: u32 = 0x0001;
    pub const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
    pub const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;

    /// The start of EXCEPTION_RECORD; the code is all that is read.
    #[repr(C)]
    pub struct ExceptionRecord {
        pub code: u32,
    }

    /// EXCEPTION_POINTERS.
    #[repr(C)]
    pub struct ExceptionPointers {
        pub record: *const ExceptionRecord,
        pub _context: *const c_void,
    }

    pub type ExceptionFilter = unsafe extern "system" fn(*const ExceptionPointers) -> i32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetStdHandle(which: u32) -> *mut c_void;
        pub fn SetStdHandle(which: u32, handle: *mut c_void) -> i32;
        pub fn SetErrorMode(mode: u32) -> u32;
        pub fn SetUnhandledExceptionFilter(filter: Option<ExceptionFilter>) -> *mut c_void;
    }
}

/// Where a crash in native code ends (a PDF or OCR library on a damaged
/// file): the process terminates with the exception code as its exit code.
#[cfg(windows)]
unsafe extern "system" fn crashed(info: *const kernel32::ExceptionPointers) -> i32 {
    let code = unsafe { info.as_ref().and_then(|i| i.record.as_ref()).map(|r| r.code) };
    die(code.unwrap_or(0xE000_0001))
}

/// Idle CPU priority, and no dialog or error reporter when a file crashes this process.
fn be_a_quiet_neighbour() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, IDLE_PRIORITY_CLASS, SetPriorityClass};
        let _ = SetPriorityClass(GetCurrentProcess(), IDLE_PRIORITY_CLASS);
        kernel32::SetErrorMode(kernel32::SEM_FAILCRITICALERRORS | kernel32::SEM_NOGPFAULTERRORBOX | kernel32::SEM_NOOPENFILEERRORBOX);
        kernel32::SetUnhandledExceptionFilter(Some(crashed));
    }
}

/// Private handles to the two pipes, with the process's own standard input
/// and output pointed elsewhere.
#[cfg(windows)]
fn take_pipes() -> Result<(File, File)> {
    use std::os::windows::io::{AsHandle, AsRawHandle, IntoRawHandle};
    use windows::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, INVALID_HANDLE_VALUE, SetHandleInformation};

    unsafe extern "C" {
        /// The C runtime keeps its own copy of the standard handles.
        fn _dup2(from: i32, to: i32) -> i32;
    }

    let (stdin, stdout) = (std::io::stdin(), std::io::stdout());
    if stdin.as_raw_handle().is_null() || stdout.as_raw_handle().is_null() {
        bail!("this process has no standard input and output; a reader is started by the worker");
    }
    let input = File::from(stdin.as_handle().try_clone_to_owned().context("standard input cannot be used")?);
    let output = File::from(stdout.as_handle().try_clone_to_owned().context("standard output cannot be used")?);
    unsafe {
        // The originals stay open but lead nowhere any more, and a program the
        // handler starts does not inherit them: a pipe end held by a grandchild
        // would keep the worker from seeing this process end.
        let _ = SetHandleInformation(HANDLE(stdin.as_raw_handle()), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
        let _ = SetHandleInformation(HANDLE(stdout.as_raw_handle()), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
        if let Ok(nul) = File::open("NUL") {
            kernel32::SetStdHandle(kernel32::STD_INPUT_HANDLE, nul.into_raw_handle());
        }
        // Standard output of C code first (it may move the Win32 handle too), then Rust's.
        let _ = _dup2(2, 1);
        let mut elsewhere = kernel32::GetStdHandle(kernel32::STD_ERROR_HANDLE);
        if (elsewhere.is_null() || elsewhere == INVALID_HANDLE_VALUE.0)
            && let Ok(nul) = std::fs::OpenOptions::new().write(true).open("NUL")
        {
            elsewhere = nul.into_raw_handle();
        }
        kernel32::SetStdHandle(kernel32::STD_OUTPUT_HANDLE, elsewhere);
    }
    Ok((input, output))
}

#[cfg(not(windows))]
fn take_pipes() -> Result<(File, File)> {
    use std::os::fd::AsFd;
    let input = File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let output = File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    Ok((input, output))
}
