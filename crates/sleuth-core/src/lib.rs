//! Shared types and pure logic for Sleuth: the tag-aware matcher, the
//! drive-letter / UNC map, persisted settings and the worker IPC protocol.

pub mod analyzer;
pub mod paths;
pub mod protocol;
pub mod settings;

/// Product name used for folders, pipe names and window titles.
pub const APP_NAME: &str = "Sleuth";
