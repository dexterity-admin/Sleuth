//! Where tests write: under `SLEUTH_TEST_DIR` when set (a share, to test
//! there; or simply a disk with room), else the temporary folder.

use std::path::PathBuf;

/// A folder of this test's own, not yet created.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let root = std::env::var_os("SLEUTH_TEST_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let d = root.join(format!("sl-office-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}
