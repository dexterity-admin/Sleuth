//! Reader processes at idle priority, and the governor that decides how many run.
//!
//! The background worker reads documents in *reader processes*: children at
//! idle CPU priority, so a file that crashes or hangs its reader costs one
//! process and not the worker. They run in lanes ("fast" for text extraction,
//! "ocr" for the slow, CPU-heavy reading), and the [`Governor`] decides how
//! many of each run, so reading never stops on a busy PC and speeds up
//! quickly on a free one.
//!
//! - [`meters`]: what the PC is doing (CPU used by others, idle time, battery, free memory).
//! - [`governor`]: from those readings to a number of workers per lane. No I/O, fully testable.
//! - [`group`]: the Windows job object that holds every reader process.
//! - [`workers`]: the parent side: start, feed, time out and replace readers.
//! - [`child`]: the child side: [`serve`] turns a process into a reader.
//!
//! The job, reply and settings types belong to the caller; this crate only
//! moves them as JSON, one object per line.

pub mod child;
pub mod governor;
pub mod group;
pub mod meters;
mod wire;
pub mod workers;

pub use child::serve;
pub use governor::{Governor, Inputs, Speed, Targets};
pub use group::ProcessGroup;
pub use meters::{CpuMeter, cores, free_ram_mb, idle_seconds, on_battery};
pub use workers::{Event, Handler, Spawn, Workers};
