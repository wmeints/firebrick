//! Host paths in the spec: resolves the `host` of each mount and the mise `config` directory
//! relative to the spec's directory, expanding `~` and `~user`, and checks they exist.

use anyhow::{Context, Result, anyhow, bail};
use firebrick_spec::{MiseSpec, MountSpec};
use std::io;
use std::path::{Path, PathBuf};

use crate::api::Mount;

/// Turns the mounts of a spec into their API messages, with canonical absolute host paths.
pub(super) fn resolve_mounts(
    mounts: Vec<MountSpec>,
    spec_dir: &Path,
    home: Option<&std::ffi::OsStr>,
) -> Result<Vec<Mount>> {
    mounts
        .into_iter()
        .map(|mount| {
            let host = resolve_host_path(&mount.host, spec_dir, home)?;

            Ok(Mount {
                host: host.to_string_lossy().into_owned(),
                guest: mount.guest,
                readonly: mount.readonly,
            })
        })
        .collect()
}

/// Returns the canonical `mise.config` directory for the start request, or an empty path when
/// mise is disabled or the spec sets no dotfiles directory with a `mise.toml`.
pub(super) fn mise_config(
    mise: &MiseSpec,
    spec_dir: &Path,
    home: Option<&std::ffi::OsStr>,
) -> Result<String> {
    let dir = match mise.config.as_deref() {
        Some(config) if mise.is_enabled() => dotfiles_dir(config, spec_dir, home)?,
        _ => None,
    };

    Ok(dir
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default())
}

/// Resolves the `mise.config` directory like a mount host path. Returns `None`, with a warning
/// on stderr, when the directory has no `mise.toml`, so the sandbox starts without dotfiles.
fn dotfiles_dir(
    config: &str,
    spec_dir: &Path,
    home: Option<&std::ffi::OsStr>,
) -> Result<Option<PathBuf>> {
    let dir = resolve_host_path(config, spec_dir, home)?;
    let config = dir.join("mise.toml");
    let exists = config
        .try_exists()
        .with_context(|| format!("can't check for {}", config.display()))?;

    if exists {
        return Ok(Some(dir));
    }

    eprintln!(
        "warning: {} has no mise.toml, so the sandbox gets no dotfiles",
        dir.display()
    );

    Ok(None)
}

/// Resolves a mount host path to the canonical path of an existing directory. A leading `~`
/// expands to the home directory, `~user` to the home directory of that user, and a relative
/// path resolves against the spec file's directory.
fn resolve_host_path(
    host: &str,
    spec_dir: &Path,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    let path = match host.strip_prefix('~') {
        Some(rest) => expand_tilde(host, rest, home)?,
        None => spec_dir.join(host),
    };

    let canonical = dunce::canonicalize(&path).map_err(|err| mount_error(&path, &err))?;

    if !canonical.is_dir() {
        bail!("can't mount {}: not a directory", canonical.display());
    }

    Ok(canonical)
}

/// The characters that end the `~user` at the start of a host path.
#[cfg(unix)]
const SEPARATORS: &[char] = &['/'];

/// The characters that end the `~user` at the start of a host path.
#[cfg(windows)]
const SEPARATORS: &[char] = &['/', '\\'];

/// Expands `~` or `~user` at the start of a host path, the way a shell does. `rest` is the
/// host path without its `~`. Refuses a relative home directory, which would silently resolve
/// against the current directory.
fn expand_tilde(host: &str, rest: &str, home: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    let (user, tail) = rest.split_once(SEPARATORS).unwrap_or((rest, ""));

    let (home, source) = if user.is_empty() {
        (
            own_home(host, home)?,
            format!("${}", firebrick_utils::HOME_VAR),
        )
    } else {
        (
            user_home(host, user)?,
            format!("the home directory of {user}"),
        )
    };

    if !home.is_absolute() {
        bail!("can't expand ~{user} in {host}: {source} is {home:?}, not an absolute path");
    }

    Ok(home.join(tail))
}

/// Returns the home directory from the environment.
fn own_home(host: &str, home: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    let home = home.with_context(|| {
        format!(
            "can't expand ~ in {host}: ${} isn't set",
            firebrick_utils::HOME_VAR
        )
    })?;

    Ok(PathBuf::from(home))
}

/// Looks up the home directory of the user in the user database.
#[cfg(unix)]
fn user_home(host: &str, user: &str) -> Result<PathBuf> {
    let entry = nix::unistd::User::from_name(user)
        .with_context(|| format!("can't expand ~{user} in {host}"))?
        .ok_or_else(|| anyhow!("can't expand {host}: user {user} doesn't exist"))?;

    Ok(entry.dir)
}

/// Fails: Windows has no user database to look up the home directory of `~user` in.
#[cfg(windows)]
fn user_home(host: &str, user: &str) -> Result<PathBuf> {
    bail!("can't expand ~{user} in {host}: ~user isn't supported on Windows; use ~ or a path")
}

/// Describes why the host path can't be mounted, without the OS error code.
fn mount_error(path: &Path, err: &io::Error) -> anyhow::Error {
    let reason = match err.kind() {
        io::ErrorKind::NotFound => "No such file or directory".to_string(),
        _ => err.to_string(),
    };

    anyhow!("can't mount {}: {reason}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn host_path_resolves_absolute_path() {
        let dir = TempDir::new().unwrap();
        let host = dir.path().to_str().unwrap();

        let resolved = resolve_host_path(host, Path::new("/elsewhere"), None).unwrap();

        assert_eq!(resolved, dunce::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn host_path_resolves_relative_to_spec_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("project")).unwrap();
        fs::create_dir_all(dir.path().join("shared")).unwrap();

        let resolved = resolve_host_path("../shared", &dir.path().join("project"), None).unwrap();

        assert_eq!(
            resolved,
            dunce::canonicalize(dir.path().join("shared")).unwrap()
        );
    }

    #[test]
    fn host_path_expands_home() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("datasets")).unwrap();
        let home = Some(dir.path().as_os_str());

        let resolved = resolve_host_path("~/datasets", Path::new("/elsewhere"), home).unwrap();
        let home_only = resolve_host_path("~", Path::new("/elsewhere"), home).unwrap();

        assert_eq!(
            resolved,
            dunce::canonicalize(dir.path().join("datasets")).unwrap()
        );
        assert_eq!(home_only, dunce::canonicalize(dir.path()).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn host_path_expands_other_users_home() {
        // root exists on every Linux and macOS host; its home differs between them.
        let root_home = nix::unistd::User::from_name("root").unwrap().unwrap().dir;

        let resolved = resolve_host_path("~root", Path::new("/elsewhere"), None).unwrap();

        assert_eq!(resolved, dunce::canonicalize(root_home).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn host_path_with_unknown_user_is_refused() {
        let error =
            resolve_host_path("~fbk-no-such-user/data", Path::new("/elsewhere"), None).unwrap_err();

        assert_eq!(
            error.to_string(),
            "can't expand ~fbk-no-such-user/data: user fbk-no-such-user doesn't exist"
        );
    }

    #[test]
    fn host_path_needs_home_for_tilde() {
        let error = resolve_host_path("~/datasets", Path::new("/elsewhere"), None).unwrap_err();

        let missing = format!("${} isn't set", firebrick_utils::HOME_VAR);
        assert!(error.to_string().contains(&missing), "{error}");
    }

    #[test]
    fn host_path_with_relative_home_is_refused() {
        let relative = Some(std::ffi::OsStr::new("."));
        let empty = Some(std::ffi::OsStr::new(""));

        let relative_error = resolve_host_path("~", Path::new("/elsewhere"), relative).unwrap_err();
        let empty_error = resolve_host_path("~", Path::new("/elsewhere"), empty).unwrap_err();

        let expected = |home: &str| {
            format!(
                "can't expand ~ in ~: ${} is {home:?}, not an absolute path",
                firebrick_utils::HOME_VAR
            )
        };
        assert_eq!(relative_error.to_string(), expected("."));
        assert_eq!(empty_error.to_string(), expected(""));
    }

    #[cfg(windows)]
    #[test]
    fn host_path_of_another_user_is_refused_on_windows() {
        let error = resolve_host_path("~bob/data", Path::new(r"C:\elsewhere"), None).unwrap_err();

        assert_eq!(
            error.to_string(),
            "can't expand ~bob in ~bob/data: ~user isn't supported on Windows; use ~ or a path"
        );
    }

    #[cfg(windows)]
    #[test]
    fn host_path_expands_home_with_a_backslash() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("datasets")).unwrap();
        let home = Some(dir.path().as_os_str());

        let resolved = resolve_host_path(r"~\datasets", Path::new(r"C:\elsewhere"), home).unwrap();

        assert_eq!(
            resolved,
            dunce::canonicalize(dir.path().join("datasets")).unwrap()
        );
    }

    #[test]
    fn missing_host_path_is_refused() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing");

        let error = resolve_host_path("missing", dir.path(), None).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "can't mount {}: No such file or directory",
                missing.display()
            )
        );
    }

    #[test]
    fn host_path_that_is_a_file_is_refused() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file"), "").unwrap();
        let file = dunce::canonicalize(dir.path().join("file")).unwrap();

        let error = resolve_host_path("file", dir.path(), None).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("can't mount {}: not a directory", file.display())
        );
    }
}
