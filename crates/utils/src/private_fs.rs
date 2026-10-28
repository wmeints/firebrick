//! Files and directories only the current user can open: mode `0600` and `0700` on Unix, and an
//! access list for the user and the system on Windows.

#[cfg(windows)]
use crate::win32;
use std::fs;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Creates the directory with mode `0700` when it's missing, and checks that it's private to
/// the effective user: a directory, not a symlink, owned by the user and without group or
/// other access. Fails with [`io::ErrorKind::PermissionDenied`] when it isn't, so the caller
/// never uses a directory that another user created first. Every error names the directory.
#[cfg(unix)]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    create_private_dir_for(dir, nix::unistd::geteuid().as_raw())
}

/// Creates the directory like [`create_private_dir`] and checks that it's private to `uid`.
#[cfg(unix)]
fn create_private_dir_for(dir: &Path, uid: u32) -> io::Result<()> {
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        // Something else is in its place, which the check below refuses.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(with_path(err, "can't create", dir)),
        Ok(()) => {}
    }

    let metadata = fs::symlink_metadata(dir).map_err(|err| with_path(err, "can't inspect", dir))?;
    let problem = if !metadata.is_dir() {
        "isn't a directory"
    } else if metadata.uid() != uid {
        "is owned by another user"
    } else if metadata.mode() & 0o077 != 0 {
        "is accessible by other users"
    } else {
        return Ok(());
    };

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} {problem}; remove it or make it a directory only you can access",
            dir.display()
        ),
    ))
}

/// Returns the error with what failed for which path in front of its message.
#[cfg(unix)]
fn with_path(err: io::Error, action: &str, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{action} {}: {err}", path.display()))
}

/// Replaces a file with `content` that only the user can read, without leaving a partial file.
/// On Windows the file inherits the access list of its directory, which in the user's profile
/// grants access to the user, administrators and `LocalSystem` only.
pub fn write_private(path: &Path, content: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }

    let temp_path = path.with_extension("tmp");
    let mut file = open_private(&temp_path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;

    fs::rename(&temp_path, path)
}

/// Creates or truncates the file with mode `0600`. The mode only applies to new files, so this
/// also fixes a file left over with other permissions.
#[cfg(unix)]
fn open_private(path: &Path) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;

    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Creates or truncates the file and gives it an access list for the user, the system and the
/// administrators only, so it's private also outside the user's profile.
#[cfg(windows)]
fn open_private(path: &Path) -> io::Result<fs::File> {
    let file = fs::File::create(path)?;
    win32::restrict_to_owner(path, &win32::Sid::current_user()?)?;
    Ok(file)
}

/// Creates the directory when it's missing and makes it accessible only to the user: mode
/// `0700` on Unix, and on Windows a protected access list for the user, the system and the
/// administrators, which the files created in it inherit. Unlike [`create_private_dir`] on Unix,
/// it takes over a directory the user owns instead of refusing one that isn't private yet.
pub fn make_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir).map_err(|err| {
        io::Error::new(err.kind(), format!("can't create {}: {err}", dir.display()))
    })?;
    restrict_dir(dir).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("can't make {} private: {err}", dir.display()),
        )
    })
}

/// Gives the directory mode `0700`.
#[cfg(unix)]
fn restrict_dir(dir: &Path) -> io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Gives the directory an access list for the user, the system and the administrators only.
#[cfg(windows)]
fn restrict_dir(dir: &Path) -> io::Result<()> {
    win32::restrict_to_owner(dir, &win32::Sid::current_user()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_private_replaces_the_file_with_a_private_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config/approved.yml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path.with_extension("tmp"), "left over").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            path.with_extension("tmp"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        write_private(&path, "first").unwrap();
        write_private(&path, "second").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        assert!(!path.with_extension("tmp").exists());
        // On Windows, `windows_tests::write_private_protects_the_file` checks the access list.
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[test]
    fn create_private_dir_creates_a_directory_only_the_user_can_access() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("firebrick/netlog");

        create_private_dir(&dir).unwrap();
        create_private_dir(&dir).unwrap();

        let mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o700);
    }

    #[test]
    fn create_private_dir_refuses_a_directory_with_group_or_other_access() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("netlog");
        fs::create_dir(&dir).unwrap();

        for mode in [0o750, 0o705, 0o1777] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
            let err = create_private_dir(&dir).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{mode:o}");
        }
    }

    #[test]
    fn create_private_dir_refuses_a_directory_of_another_user() {
        let temp = tempfile::TempDir::new().unwrap();
        let uid = fs::metadata(temp.path()).unwrap().uid();

        let err = create_private_dir_for(temp.path(), uid.wrapping_add(1)).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains(&temp.path().display().to_string()));
    }

    /// Asserts that the path is refused as no directory.
    fn assert_not_a_directory(path: &Path) {
        let err = create_private_dir(path).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(err.to_string().contains("isn't a directory"), "{err}");
    }

    #[test]
    fn create_private_dir_refuses_a_symlink_to_a_private_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("target");
        create_private_dir(&target).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_not_a_directory(&link);
    }

    #[test]
    fn create_private_dir_refuses_a_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

        assert_not_a_directory(&file);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::windows_tests::{assert_private, outside_profile, sddl};

    #[test]
    fn make_private_dir_protects_a_directory_and_what_is_created_in_it() {
        let temp = outside_profile();
        let dir = temp.path().join("firebrick/data");

        make_private_dir(&dir).unwrap();
        fs::write(dir.join("secrets.yml"), "x").unwrap();

        let acl = sddl(&dir);
        assert!(acl.contains("D:P"), "not protected: {acl}");
        assert_private(&acl);
        assert_private(&sddl(&dir.join("secrets.yml")));
    }

    #[test]
    fn write_private_protects_the_file() {
        let temp = outside_profile();
        let path = temp.path().join("approved.yml");

        write_private(&path, "approved").unwrap();

        let acl = sddl(&path);
        assert!(acl.contains("D:P"), "not protected: {acl}");
        assert_private(&acl);
    }
}
