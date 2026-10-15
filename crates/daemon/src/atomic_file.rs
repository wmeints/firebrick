//! Replacing a file atomically, so readers see either its old or its new content.
//!
//! Shared by the modules that edit the user's own config files, such as the editors' settings
//! and `~/.ssh/config`.

use std::fs;
use std::io::{self, ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Writes a temporary file next to `path` and renames it over the file, so a failed write
/// leaves the old content in place. A symlinked file (dotfiles) stays a symlink, because the
/// rename replaces the file it points to. An existing file keeps its permissions; a new one is
/// created with `new_mode`, minus the umask, or on Windows with the access list it inherits
/// from its directory.
pub(crate) fn write(path: &Path, content: &str, new_mode: u32) -> io::Result<()> {
    let target = match fs::canonicalize(path) {
        Ok(target) => target,
        Err(err) if err.kind() == ErrorKind::NotFound => path.to_path_buf(),
        Err(err) => return Err(err),
    };
    let file_name = target.file_name().unwrap_or_default().to_string_lossy();
    let temp = target.with_file_name(format!(".{file_name}.firebrick-tmp"));

    let result = write_new(&temp, content, new_mode)
        .and_then(|()| keep_permissions(&target, &temp))
        .and_then(|()| fs::rename(&temp, &target));

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }

    result
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

        write(&path, "new\n", 0o600).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn replaces_target_of_symlink() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::write(&target, "old\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write(&link, "new\n", 0o600).unwrap();

        assert!(link.is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
    }
}
