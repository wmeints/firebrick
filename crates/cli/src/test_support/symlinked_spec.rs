//! Workspaces for unit tests whose `.firebrick.yml` is a symlink, to check that commands refuse
//! it and leave its target alone, and the symlinks they're made of.
//!
//! Creating a symlink on Windows takes administrator rights or Developer Mode. Without them,
//! the helpers return `None` or `false` so the test can stop, unless `FIREBRICK_TEST_SYMLINKS`
//! is set, as in CI, which fails the test instead.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::spec_file::SPEC_FILE_NAME;

/// The error every command fails with when `.firebrick.yml` is a symlink.
pub const SYMLINK_ERROR: &str =
    ".firebrick.yml is a symlink; firebrick only uses a regular file in the workspace";

/// Returns a workspace whose `.firebrick.yml` links to a file next to it, and the path of that
/// file. The file holds `content`, or doesn't exist when it is `None`, so the link dangles.
/// Returns `None` when this Windows can't create symlinks.
pub fn workspace(content: Option<&str>) -> Option<(TempDir, PathBuf)> {
    // Only exists in tests, where a failure should fail the test.
    // foxguard: ignore[rs/no-unwrap-in-lib]
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("notes.yml");

    if let Some(content) = content {
        // Only exists in tests, where a failure should fail the test.
        // foxguard: ignore[rs/no-unwrap-in-lib]
        fs::write(&target, content).unwrap();
    }

    symlink_file(&target, &dir.path().join(SPEC_FILE_NAME)).then_some((dir, target))
}

/// Returns the content of the link's target, or `None` when it doesn't exist.
pub fn target_content(target: &Path) -> Option<String> {
    fs::read_to_string(target).ok()
}

/// Creates `link` as a symlink to the file `target`, which may not exist. Returns `false`
/// when this Windows can't create symlinks.
pub fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file(target, link);

    created_or_stop(created)
}

/// Creates `link` as a symlink to the directory `target`. Returns `false` when this Windows
/// can't create symlinks.
pub fn symlink_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_dir(target, link);

    created_or_stop(created)
}

/// Returns whether the symlink was created. Panics when it wasn't, unless this is Windows
/// without `FIREBRICK_TEST_SYMLINKS`.
fn created_or_stop(created: io::Result<()>) -> bool {
    match created {
        Ok(()) => true,
        Err(err) if cfg!(windows) && std::env::var_os("FIREBRICK_TEST_SYMLINKS").is_none() => {
            eprintln!(
                "can't create a symlink ({err}), so the test stops; set \
                 FIREBRICK_TEST_SYMLINKS=1 to fail it"
            );
            false
        }
        Err(err) => panic!("can't create a symlink: {err}"),
    }
}
