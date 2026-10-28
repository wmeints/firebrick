//! Reads and writes spec files without following a symlink, so an agent can't point the spec
//! at a host file.

use super::error::SandboxSpecError;
use super::{SandboxSpec, from_str};
use std::fs;
use std::io::{ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use std::result::Result;

/// Loads and parses a sandbox spec from a YAML file, or returns `None` when it doesn't exist.
/// Refuses a symlink and other files that aren't regular files, like [`read_text`].
pub fn from_file(path: &Path) -> Result<Option<SandboxSpec>, SandboxSpecError> {
    read_text(path)?.map(|text| from_str(&text)).transpose()
}

/// Reads the text of a spec file, or returns `None` when it doesn't exist. Fails with
/// [`SandboxSpecError::Symlink`] when the path is a symlink, dangling or not, because the agent
/// in the sandbox controls the workspace and could point the spec at any file of the user. Fails
/// with [`SandboxSpecError::NotRegularFile`] for a directory or a FIFO, which would hang the read.
pub fn read_text(path: &Path) -> Result<Option<String>, SandboxSpecError> {
    let Some(checked) = regular_file_metadata(path)? else {
        return Ok(None);
    };

    read_checked(path, &checked).map(Some)
}

/// Returns whether the spec file exists. Fails when it is a symlink or isn't a regular file.
pub fn exists(path: &Path) -> Result<bool, SandboxSpecError> {
    Ok(regular_file_metadata(path)?.is_some())
}

/// Replaces the spec file with `text`. Fails with [`SandboxSpecError::Symlink`] when the path is
/// a symlink. Writes a temporary file next to it and renames that over the path, so a symlink
/// created in the meantime is replaced instead of followed, and readers never see a partial
/// file.
pub fn write_text(path: &Path, text: &str) -> Result<(), SandboxSpecError> {
    regular_file_metadata(path)?;

    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()));
    let result = write_new(&temp, text).and_then(|()| fs::rename(&temp, path));

    if result.is_err() {
        // The write already failed; a temp file that can't be removed changes nothing about it.
        let _ = fs::remove_file(&temp);
    }

    result.map_err(|source| SandboxSpecError::CantWrite {
        path: path.to_path_buf(),
        source,
    })
}

/// Reads the file at `path`, failing when it isn't the `checked` regular file, because a symlink
/// was swapped in after the check.
fn read_checked(path: &Path, checked: &fs::Metadata) -> Result<String, SandboxSpecError> {
    let cant_read = |source| SandboxSpecError::CantRead {
        path: path.to_path_buf(),
        source,
    };
    let Some(mut file) = open_checked(path, checked).map_err(cant_read)? else {
        return Err(symlink_error(path));
    };

    let mut text = String::new();
    file.read_to_string(&mut text).map_err(cant_read)?;

    Ok(text)
}

/// Opens the file at `path`, or returns `None` when the opened file isn't the `checked` one.
#[cfg(unix)]
fn open_checked(path: &Path, checked: &fs::Metadata) -> std::io::Result<Option<fs::File>> {
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;

    Ok(((opened.dev(), opened.ino()) == (checked.dev(), checked.ino())).then_some(file))
}

/// Opens the file at `path` without following a symlink, or returns `None` when what it opened
/// isn't a regular file. Windows opens a symlink itself with `FILE_FLAG_OPEN_REPARSE_POINT`, so
/// a symlink swapped in after the check is never followed.
#[cfg(windows)]
fn open_checked(path: &Path, _checked: &fs::Metadata) -> std::io::Result<Option<fs::File>> {
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let opened = file.metadata()?;

    Ok((opened.is_file() && !opened.is_symlink()).then_some(file))
}

/// Returns the metadata of the file at `path` without following a symlink, or `None` when it
/// doesn't exist. Fails when it is a symlink or isn't a regular file.
fn regular_file_metadata(path: &Path) -> Result<Option<fs::Metadata>, SandboxSpecError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_symlink() => Err(symlink_error(path)),
        Ok(metadata) if !metadata.is_file() => {
            Err(SandboxSpecError::NotRegularFile(file_name(path)))
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SandboxSpecError::CantRead {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Writes `text` to a file that doesn't exist yet. Creating it exclusively never follows a
/// symlink at the path.
fn write_new(path: &Path, text: &str) -> std::io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(text.as_bytes())
}

/// Returns the error for a spec file at `path` that is a symlink.
fn symlink_error(path: &Path) -> SandboxSpecError {
    SandboxSpecError::Symlink(file_name(path))
}

/// Returns the file name of `path` for an error message.
fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Writes the spec to a YAML file, replacing the file's contents. Comments and formatting of an
/// existing file are lost, and fields that aren't set are left out. Refuses a symlink, like
/// [`write_text`].
pub fn to_file(spec: &SandboxSpec, path: &Path) -> Result<(), SandboxSpecError> {
    let content = serde_yaml::to_string(spec).map_err(SandboxSpecError::CantSerialize)?;

    write_text(path, &content)
}
