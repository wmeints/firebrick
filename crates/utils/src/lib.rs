//! File locations and naming rules shared by the Firebrick CLI and daemon.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Name used when a path's leaf has no ASCII letters or digits left after sanitizing.
const FALLBACK_LABEL: &str = "sandbox";

/// Error for when neither `XDG_DATA_HOME` nor `HOME` is an absolute path.
pub const NO_DATA_DIR: &str =
    "can't choose a data directory: set HOME or XDG_DATA_HOME to an absolute path";

/// Error for when neither `XDG_STATE_HOME` nor `HOME` is an absolute path.
pub const NO_LOG_DIR: &str =
    "can't choose a log directory: set HOME or XDG_STATE_HOME to an absolute path";

/// Returns the path of the daemon's unix socket.
pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("fbkd.sock")
}

/// Returns why the daemon can't choose its data or log directory in this environment, or
/// `None` when it can. The CLI checks this before it starts the daemon, because the daemon's
/// own error doesn't reach the user.
pub fn missing_dir() -> Option<&'static str> {
    missing_dir_from(&env_var)
}

/// Returns why there's no data or log directory in the environment that `var` describes.
fn missing_dir_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<&'static str> {
    if data_dir_from(var).is_none() {
        Some(NO_DATA_DIR)
    } else if log_dir_from(var).is_none() {
        Some(NO_LOG_DIR)
    } else {
        None
    }
}

/// Returns whether a peer on the daemon socket running as `peer_uid` is trusted by a process
/// running as `own_uid`: only the same user and root are. The daemon applies this to its
/// clients and the CLI to the daemon. Root is trusted because it can bypass the check anyway,
/// for example by reading the other process's memory.
pub fn is_trusted_peer(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid || peer_uid == 0
}

/// Returns the directory the daemon writes its log files to: `$XDG_STATE_HOME/firebrick`,
/// falling back to `$HOME/.local/state/firebrick`.
///
/// Returns `None` when neither variable is an absolute path, so the logs never end up in a
/// temp directory that other users can create first.
pub fn log_dir() -> Option<PathBuf> {
    log_dir_from(&env_var)
}

/// Returns the log directory for the environment that `var` describes.
fn log_dir_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    base_dir(var, "XDG_STATE_HOME", ".local/state").map(|state| state.join("firebrick"))
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
    log_dir_from(var).map(|dir| dir.join("msb"))
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

/// Returns the directory holding the SSH keys and config the daemon provisions for sandboxes,
/// or `None` when there's no data directory (see `data_dir`).
pub fn ssh_dir() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("ssh"))
}

/// Returns the file the daemon stores the secrets for sandboxes in, or `None` when there's no
/// data directory (see `data_dir`).
pub fn secrets_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("secrets.yml"))
}

/// Returns the directory holding the daemon's persistent data: `$XDG_DATA_HOME/firebrick`,
/// falling back to `$HOME/.local/share/firebrick`.
///
/// Returns `None` when neither variable is an absolute path, so the secrets and SSH keys never
/// end up in a temp directory that other users can create first.
fn data_dir() -> Option<PathBuf> {
    data_dir_from(&env_var)
}

/// Returns the data directory for the environment that `var` describes.
fn data_dir_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    base_dir(var, "XDG_DATA_HOME", ".local/share").map(|data| data.join("firebrick"))
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

    /// Returns the data, log and microsandbox directories for an environment holding `vars`.
    fn dirs(vars: &[(&str, &str)]) -> [Option<PathBuf>; 3] {
        let var = env(vars);
        [data_dir_from(&var), log_dir_from(&var), msb_home_from(&var)]
    }

    /// Returns the expected directories, all `Some`.
    fn some(data: &str, log: &str, msb: &str) -> [Option<PathBuf>; 3] {
        [data, log, msb].map(|dir| Some(PathBuf::from(dir)))
    }

    /// Returns the directories expected under `/home/user` when no XDG variable is usable.
    fn in_home() -> [Option<PathBuf>; 3] {
        some(
            "/home/user/.local/share/firebrick",
            "/home/user/.local/state/firebrick",
            "/home/user/.local/state/firebrick/msb",
        )
    }

    #[test]
    fn dirs_are_under_xdg_dirs() {
        let vars = [
            ("XDG_DATA_HOME", "/data"),
            ("XDG_STATE_HOME", "/state"),
            ("HOME", "/home/user"),
        ];
        let expected = some(
            "/data/firebrick",
            "/state/firebrick",
            "/state/firebrick/msb",
        );
        assert_eq!(dirs(&vars), expected);
    }

    #[test]
    fn dirs_fall_back_to_home_for_missing_empty_and_relative_xdg_dirs() {
        for (data, state) in [
            (None, None),
            (Some(""), Some("state")),
            (Some("./data"), Some("")),
        ] {
            let mut vars = vec![("HOME", "/home/user")];
            vars.extend(data.map(|data| ("XDG_DATA_HOME", data)));
            vars.extend(state.map(|state| ("XDG_STATE_HOME", state)));
            assert_eq!(dirs(&vars), in_home(), "{vars:?}");
        }
    }

    #[test]
    fn dirs_are_none_without_absolute_home() {
        let cases = [
            &[][..],
            &[("HOME", "")],
            &[
                ("HOME", "home"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_STATE_HOME", "state"),
            ],
        ];
        for vars in cases {
            assert_eq!(dirs(vars), [None, None, None], "{vars:?}");
        }
    }

    #[test]
    fn missing_dir_names_the_first_missing_variable() {
        let cases = [
            (&[("HOME", "/home/user")][..], None),
            (&[], Some(NO_DATA_DIR)),
            (&[("XDG_STATE_HOME", "/state")], Some(NO_DATA_DIR)),
            (&[("XDG_DATA_HOME", "/data")], Some(NO_LOG_DIR)),
        ];
        for (vars, expected) in cases {
            assert_eq!(missing_dir_from(&env(vars)), expected, "{vars:?}");
        }
    }

    #[test]
    fn trusts_only_own_uid_and_root() {
        let cases = [(1000, true), (0, true), (1001, false), (u32::MAX, false)];
        for (peer_uid, expected) in cases {
            assert_eq!(
                is_trusted_peer(peer_uid, 1000),
                expected,
                "peer uid {peer_uid}"
            );
        }
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
