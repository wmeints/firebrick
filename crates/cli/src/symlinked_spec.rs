//! Workspaces for unit tests whose `.firebrick.yml` is a symlink, to check that commands refuse
//! it and leave its target alone.

use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::manage::SPEC_FILE_NAME;

/// The error every command fails with when `.firebrick.yml` is a symlink.
pub const SYMLINK_ERROR: &str =
    ".firebrick.yml is a symlink; firebrick only uses a regular file in the workspace";

/// Returns a workspace whose `.firebrick.yml` links to a file next to it, and the path of that
/// file. The file holds `content`, or doesn't exist when it is `None`, so the link dangles.
pub fn workspace(content: Option<&str>) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("notes.yml");

    if let Some(content) = content {
        fs::write(&target, content).unwrap();
    }

    std::os::unix::fs::symlink(&target, dir.path().join(SPEC_FILE_NAME)).unwrap();
    (dir, target)
}

/// Returns the content of the link's target, or `None` when it doesn't exist.
pub fn target_content(target: &Path) -> Option<String> {
    fs::read_to_string(target).ok()
}
