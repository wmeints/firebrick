//! File locations and naming rules shared by the Firebrick CLI and daemon.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Name used when a path's leaf has no ASCII letters or digits left after sanitizing.
const FALLBACK_LABEL: &str = "sandbox";

/// Returns the path of the daemon's unix socket.
pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("fbkd.sock")
}

/// Returns the directory the daemon writes its log files to.
pub fn log_dir() -> PathBuf {
    base_dir(&env_var, "XDG_STATE_HOME", ".local/state")
        .unwrap_or_else(std::env::temp_dir)
        .join("firebrick")
}

/// Returns the microsandbox home the daemon uses when `MSB_HOME` isn't set, so it never shares
/// the runtime, database and sandboxes of a separately installed `msb` in `~/.microsandbox`:
/// `$XDG_STATE_HOME/firebrick/msb`, falling back to `$HOME/.local/state/firebrick/msb`.
///
/// Returns `None` when neither variable is an absolute path. The home holds the `msb` binary
/// the daemon runs, so it must not end up in the current directory, which is often a workspace
/// mounted into a sandbox, or in a temp directory that other users can create first. The name
/// is short because microsandbox creates unix sockets under the home.
pub fn msb_home() -> Option<PathBuf> {
    msb_home_from(&env_var)
}

/// Returns the firebrick microsandbox home for the environment that `var` describes.
fn msb_home_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    base_dir(var, "XDG_STATE_HOME", ".local/state").map(|state| state.join("firebrick/msb"))
}

/// Returns the XDG base directory in `xdg_var`, falling back to `home_subdir` in `$HOME`, with
/// environment variables looked up through `var`. Empty and relative values are ignored, as
/// the XDG Base Directory spec requires.
fn base_dir(
    var: &impl Fn(&str) -> Option<OsString>,
    xdg_var: &str,
    home_subdir: &str,
) -> Option<PathBuf> {
    absolute_var(var, xdg_var)
        .or_else(|| absolute_var(var, "HOME").map(|home| home.join(home_subdir)))
}

/// Returns the value of the environment variable `key` when it's an absolute path.
fn absolute_var(var: &impl Fn(&str) -> Option<OsString>, key: &str) -> Option<PathBuf> {
    var(key)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Looks up an environment variable of the current process.
fn env_var(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

/// Returns the directory holding the SSH keys and config the daemon provisions for sandboxes.
pub fn ssh_dir() -> PathBuf {
    data_dir().join("ssh")
}

/// Returns the file the daemon stores the secrets for sandboxes in.
pub fn secrets_path() -> PathBuf {
    data_dir().join("secrets.yml")
}

/// Returns the file `fbk` records the approved mounts and network settings of each workspace
/// in: `$XDG_CONFIG_HOME/firebrick/approved.yml`, falling back to
/// `$HOME/.config/firebrick/approved.yml`.
///
/// Returns `None` when neither variable is an absolute path. Sandboxes mount the workspace, so
/// the file must never end up in the current directory or a temp directory, where an agent
/// could write it.
pub fn approvals_path() -> Option<PathBuf> {
    approvals_path_from(&env_var)
}

/// Returns the approvals file for the environment that `var` describes.
fn approvals_path_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    base_dir(var, "XDG_CONFIG_HOME", ".config").map(|config| config.join("firebrick/approved.yml"))
}

/// Returns the directory holding the daemon's persistent data.
fn data_dir() -> PathBuf {
    base_dir(&env_var, "XDG_DATA_HOME", ".local/share")
        .unwrap_or_else(std::env::temp_dir)
        .join("firebrick")
}

/// Replaces a file with `content` that only the user can read, without leaving a partial file.
pub fn write_private(path: &Path, content: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }

    let temp_path = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp_path)?;

    // The mode only applies to new files, so fix a temp file left over with other permissions.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;

    fs::rename(&temp_path, path)
}

/// Turns the leaf directory of `path` into a DNS label of lowercase ASCII letters, digits and
/// dashes: every run of other characters becomes one `-`, and leading and trailing dashes are
/// dropped. Returns `sandbox` when nothing is left, for example for `/`.
pub fn sanitize_label(path: &Path) -> String {
    let leaf = path
        .file_name()
        .map(|leaf| leaf.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let mut label = String::new();

    for c in leaf.chars() {
        if c.is_ascii_alphanumeric() {
            label.push(c);
        } else if !label.is_empty() && !label.ends_with('-') {
            label.push('-');
        }
    }

    match label.trim_end_matches('-') {
        "" => FALLBACK_LABEL.to_string(),
        label => label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a lookup for an environment that holds only `vars`.
    fn env<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn msb_home_is_under_xdg_state_home() {
        let var = env(&[("XDG_STATE_HOME", "/state"), ("HOME", "/home/user")]);
        assert_eq!(
            msb_home_from(&var),
            Some(PathBuf::from("/state/firebrick/msb"))
        );
    }

    #[test]
    fn msb_home_falls_back_to_local_state_in_home() {
        let var = env(&[("HOME", "/home/user")]);
        assert_eq!(
            msb_home_from(&var),
            Some(PathBuf::from("/home/user/.local/state/firebrick/msb"))
        );
    }

    #[test]
    fn msb_home_ignores_empty_and_relative_xdg_state_home() {
        for state in ["", "state", "./state"] {
            let vars = [("XDG_STATE_HOME", state), ("HOME", "/home/user")];
            assert_eq!(
                msb_home_from(&env(&vars)),
                Some(PathBuf::from("/home/user/.local/state/firebrick/msb")),
                "XDG_STATE_HOME={state:?}"
            );
        }
    }

    #[test]
    fn msb_home_is_none_without_absolute_home() {
        assert_eq!(msb_home_from(&env(&[])), None);
        assert_eq!(msb_home_from(&env(&[("HOME", "")])), None);
        let var = env(&[("XDG_STATE_HOME", "state"), ("HOME", "home")]);
        assert_eq!(msb_home_from(&var), None);
    }

    #[test]
    fn msb_home_is_next_to_the_logs() {
        if let Some(home) = msb_home() {
            assert_eq!(home, log_dir().join("msb"));
        }
    }

    #[test]
    fn approvals_path_is_under_xdg_config_home() {
        let var = env(&[("XDG_CONFIG_HOME", "/config"), ("HOME", "/home/user")]);
        assert_eq!(
            approvals_path_from(&var),
            Some(PathBuf::from("/config/firebrick/approved.yml"))
        );
    }

    #[test]
    fn approvals_path_falls_back_to_config_in_home() {
        for config in ["", "config"] {
            let vars = [("XDG_CONFIG_HOME", config), ("HOME", "/home/user")];
            assert_eq!(
                approvals_path_from(&env(&vars)),
                Some(PathBuf::from("/home/user/.config/firebrick/approved.yml")),
                "XDG_CONFIG_HOME={config:?}"
            );
        }
    }

    #[test]
    fn approvals_path_is_none_without_absolute_home() {
        assert_eq!(approvals_path_from(&env(&[])), None);
        let var = env(&[("XDG_CONFIG_HOME", "config"), ("HOME", "home")]);
        assert_eq!(approvals_path_from(&var), None);
    }

    #[test]
    fn write_private_replaces_the_file_with_a_private_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config/approved.yml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path.with_extension("tmp"), "left over").unwrap();
        fs::set_permissions(
            path.with_extension("tmp"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        write_private(&path, "first").unwrap();
        write_private(&path, "second").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!path.with_extension("tmp").exists());
    }

    fn label(path: &str) -> String {
        sanitize_label(Path::new(path))
    }

    #[test]
    fn label_uses_leaf_directory() {
        assert_eq!(label("/home/user/project"), "project");
    }

    #[test]
    fn label_is_lowercase_with_runs_replaced_by_one_dash() {
        assert_eq!(label("/home/user/My.App"), "my-app");
        assert_eq!(label("/home/user/My_Project.v2--"), "my-project-v2");
    }

    #[test]
    fn label_drops_leading_and_trailing_dashes() {
        assert_eq!(label("/home/user/my project_"), "my-project");
        assert_eq!(label("/home/user/.config"), "config");
    }

    #[test]
    fn empty_label_falls_back_to_sandbox() {
        assert_eq!(label("/home/user/__"), "sandbox");
        assert_eq!(label("/"), "sandbox");
        assert_eq!(label(""), "sandbox");
    }
}
