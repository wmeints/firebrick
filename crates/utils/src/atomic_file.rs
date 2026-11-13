//! Replacing a file atomically, so readers see either its old or its new content.

use std::fs;
use std::io::{self, ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Writes a temporary file next to `path` and renames it over the file, so a failed write
/// leaves the old content in place. A symlinked file (dotfiles) stays a symlink, because the
/// rename replaces the file it points to. An existing file keeps its permissions; a new one is
/// created with `new_mode`, minus the umask, or on Windows with the access list it inherits
/// from its directory. Writes of the same file share the temporary file
/// `.<name>.firebrick-tmp`, so the caller serializes them. When the write fails and the
/// temporary file can't be removed, the error names it.
pub fn write_atomic(path: &Path, content: &str, new_mode: u32) -> io::Result<()> {
    let target = match fs::canonicalize(path) {
        Ok(target) => target,
        Err(err) if err.kind() == ErrorKind::NotFound => path.to_path_buf(),
        Err(err) => return Err(err),
    };
    let file_name = target.file_name().unwrap_or_default().to_string_lossy();
    let temp = target.with_file_name(format!(".{file_name}.firebrick-tmp"));

    write_new(&temp, content, new_mode)
        .and_then(|()| keep_permissions(&target, &temp))
        .and_then(|()| fs::rename(&temp, &target))
        .map_err(|err| remove_temp(&temp, err))
}

/// Removes the temporary file of a failed write and returns the write's error, naming the
/// temporary file when it is left behind.
fn remove_temp(temp: &Path, err: io::Error) -> io::Error {
    match fs::remove_file(temp) {
        Ok(()) => err,
        Err(cleanup) if cleanup.kind() == ErrorKind::NotFound => err,
        Err(cleanup) => io::Error::new(
            err.kind(),
            format!(
                "{err}; the temporary file {} is left behind: {cleanup}",
                temp.display()
            ),
        ),
    }
}

/// Writes `content` to `path`, creating it with `mode` on Unix when it doesn't exist.
fn write_new(path: &Path, content: &str, mode: u32) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(mode);
    #[cfg(windows)]
    let _ = mode;

    let mut file = options.open(path)?;
    file.write_all(content.as_bytes())?;
    // Flush before the rename, so a power loss can't leave the renamed file empty.
    file.sync_all()
}

/// Gives `temp` the permissions of `target`, when `target` exists.
fn keep_permissions(target: &Path, temp: &Path) -> io::Result<()> {
    match fs::metadata(target) {
        Ok(metadata) => fs::set_permissions(temp, metadata.permissions()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_file_without_leaving_temp_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config");

        write_atomic(&path, "new\n", 0o600).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_write_leaves_the_file_and_no_temp_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config");
        fs::create_dir(&path).unwrap();

        assert!(write_atomic(&path, "new\n", 0o600).is_err());

        assert!(path.is_dir());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
