//! The daemon's process: the lock that keeps a second daemon from starting, and the standard
//! handles its children don't inherit.

#[cfg(windows)]
use crate::win32;
use std::fs;
use std::io;
use std::path::Path;

/// Opens the daemon's lock file at `path`, creating it and its directory when needed, and
/// takes its exclusive lock. Returns `None` while another process holds the lock. The lock
/// lasts until the returned file is dropped or the process exits, so `fbkd` keeps it for its
/// whole life, and `fbk` can tell when a daemon that shuts down is really gone.
pub fn try_lock_daemon(path: &Path) -> io::Result<Option<fs::File>> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }

    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(err)) => Err(err),
    }
}

/// Keeps the processes this one starts from inheriting its stdin, stdout and stderr, so a child
/// that outlives it, like the `fbkd` that `fbk` starts, doesn't hold the pipes of whatever runs
/// it open, such as the `ProxyCommand` pipes of `ssh` or a `| tail`. On Windows a child
/// inherits every inheritable handle of its parent; on Unix it only gets the stdio it's given,
/// so this does nothing there. It's best effort: a handle that can't be changed, such as a
/// missing one, is left alone.
pub fn keep_stdio_from_children() {
    #[cfg(windows)]
    {
        let _ = win32::set_inheritable(&io::stdin(), false);
        let _ = win32::set_inheritable(&io::stdout(), false);
        let _ = win32::set_inheritable(&io::stderr(), false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_lock_daemon_holds_the_lock_until_the_file_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data").join("fbkd.lock");

        let held = try_lock_daemon(&path).unwrap();
        let while_held = try_lock_daemon(&path).unwrap();
        drop(held);
        let after = try_lock_daemon(&path).unwrap();

        assert!(while_held.is_none());
        assert!(after.is_some());
    }
}
