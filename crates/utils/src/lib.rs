//! File locations, naming rules and text helpers shared by the Firebrick CLI and daemon.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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
    runtime_dir_from(&env_var).join("fbkd.sock")
}

/// Returns the directory holding the unix sockets that sandboxes report their network activity
/// to: `$XDG_RUNTIME_DIR/firebrick/netlog`, falling back to `/private/tmp/fbk-<uid>` on macOS,
/// which has no `XDG_RUNTIME_DIR`.
///
/// Returns `None` on other platforms when `XDG_RUNTIME_DIR` isn't an absolute path. Unlike
/// [`socket_path`], it never falls back to the temp directory, whose path on macOS is too long
/// for the sockets of long sandbox names. The macOS directory is in the shared `/tmp`, where
/// another user could create it first, so check it with [`create_private_dir`] before use.
pub fn netlog_socket_dir() -> Option<PathBuf> {
    let uid = nix::unistd::geteuid().as_raw();
    netlog_socket_dir_from(&env_var, cfg!(target_os = "macos"), uid)
}

/// Returns the network activity socket directory for the environment that `var` describes,
/// on macOS when `macos` is set, for the user `uid`. The macOS directory is canonical, because
/// microsandbox refuses host paths that go through a symlink, such as `/tmp`.
fn netlog_socket_dir_from(
    var: &impl Fn(&str) -> Option<OsString>,
    macos: bool,
    uid: u32,
) -> Option<PathBuf> {
    absolute_var(var, "XDG_RUNTIME_DIR")
        .map(|dir| dir.join("firebrick/netlog"))
        .or_else(|| macos.then(|| PathBuf::from(format!("/private/tmp/fbk-{uid}"))))
}

/// Returns the directory holding the unix sockets that sandboxes read the host clipboard
/// through: `$XDG_RUNTIME_DIR/firebrick/clipboard`, falling back to `/private/tmp/fbkc-<uid>` on
/// macOS, which has no `XDG_RUNTIME_DIR`.
///
/// Like [`netlog_socket_dir`], it never falls back to the temp directory and returns `None` on
/// other platforms without an absolute `XDG_RUNTIME_DIR`. The macOS directory sits next to the
/// network log's instead of inside it, so the sockets of 63-character sandbox names still fit in
/// a unix socket address. Check it with [`create_private_dir`] before use.
pub fn clipboard_socket_dir() -> Option<PathBuf> {
    let uid = nix::unistd::geteuid().as_raw();
    clipboard_socket_dir_from(&env_var, cfg!(target_os = "macos"), uid)
}

/// Returns the clipboard socket directory for the environment that `var` describes, on macOS
/// when `macos` is set, for the user `uid`. The macOS directory is canonical, like the one of
/// [`netlog_socket_dir_from`].
fn clipboard_socket_dir_from(
    var: &impl Fn(&str) -> Option<OsString>,
    macos: bool,
    uid: u32,
) -> Option<PathBuf> {
    absolute_var(var, "XDG_RUNTIME_DIR")
        .map(|dir| dir.join("firebrick/clipboard"))
        .or_else(|| macos.then(|| PathBuf::from(format!("/private/tmp/fbkc-{uid}"))))
}

/// Creates the directory with mode `0700` when it's missing, and checks that it's private to
/// the effective user:a directory, not a symlink, owned by the user and without group or
/// other access. Fails with [`io::ErrorKind::PermissionDenied`] when it isn't, so the caller
/// never uses a directory that another user created first. Every error names the directory.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    create_private_dir_for(dir, nix::unistd::geteuid().as_raw())
}

/// Creates the directory like [`create_private_dir`] and checks that it's private to `uid`.
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
fn with_path(err: io::Error, action: &str, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{action} {}: {err}", path.display()))
}

/// Returns `$XDG_RUNTIME_DIR`, falling back to the temp directory, for the environment that
/// `var` describes.
fn runtime_dir_from(var: &impl Fn(&str) -> Option<OsString>) -> PathBuf {
    var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Returns the directory the daemon records the network activity of sandboxes in:
/// `$XDG_STATE_HOME/firebrick/network`, falling back to `$HOME/.local/state/firebrick/network`.
///
/// Returns `None` when neither variable is an absolute path, like [`log_dir`].
pub fn network_log_dir() -> Option<PathBuf> {
    network_log_dir_from(&env_var)
}

/// Returns the network activity directory for the environment that `var` describes.
fn network_log_dir_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    log_dir_from(var).map(|dir| dir.join("network"))
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

/// Returns `text` without terminal escape sequences and control characters other than newline
/// and tab.
///
/// Guest output that ends up in an error message or a log line goes through this function, so a
/// workspace can't make `fbk` write terminal escape sequences, such as OSC 52 to set the
/// clipboard. [`console::strip_ansi_codes`] removes whole escape sequences, with the C1 forms of
/// CSI and OSC rewritten to their `ESC` forms first, and then the remaining C0 controls, DEL and
/// C1 controls are removed, keeping the text around them.
pub fn strip_control(text: &str) -> String {
    let text = text.replace('\u{9b}', "\x1b[").replace('\u{9d}', "\x1b]");
    console::strip_ansi_codes(&text)
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
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

    #[test]
    fn netlog_socket_dir_is_under_xdg_runtime_dir_on_every_platform() {
        let var = env(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        for macos in [false, true] {
            assert_eq!(
                netlog_socket_dir_from(&var, macos, 1000),
                Some(PathBuf::from("/run/user/1000/firebrick/netlog")),
                "macos: {macos}"
            );
        }
    }

    /// Environments without an absolute `XDG_RUNTIME_DIR`.
    const NO_RUNTIME_DIR: [&[(&str, &str)]; 3] = [
        &[],
        &[("XDG_RUNTIME_DIR", "run")],
        &[("XDG_RUNTIME_DIR", "")],
    ];

    #[test]
    fn netlog_socket_dir_is_none_on_linux_without_absolute_xdg_runtime_dir() {
        for vars in NO_RUNTIME_DIR {
            let dir = netlog_socket_dir_from(&env(vars), false, 1000);
            assert_eq!(dir, None, "{vars:?}");
        }
    }

    #[test]
    fn netlog_socket_dir_falls_back_to_a_canonical_tmp_dir_per_user_on_macos() {
        for vars in NO_RUNTIME_DIR {
            assert_eq!(
                netlog_socket_dir_from(&env(vars), true, 501),
                Some(PathBuf::from("/private/tmp/fbk-501")),
                "{vars:?}"
            );
        }
    }

    #[test]
    fn macos_netlog_socket_dir_fits_sockets_of_63_character_sandbox_names() {
        let dir = netlog_socket_dir_from(&env(&[]), true, u32::MAX).unwrap();
        let socket = dir.join(format!("{}.sock", "a".repeat(63)));

        assert!(socket.as_os_str().len() < 104, "{}", socket.display());
    }

    #[test]
    fn clipboard_socket_dir_is_under_xdg_runtime_dir_on_every_platform() {
        let var = env(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        for macos in [false, true] {
            assert_eq!(
                clipboard_socket_dir_from(&var, macos, 1000),
                Some(PathBuf::from("/run/user/1000/firebrick/clipboard")),
                "macos: {macos}"
            );
        }
    }

    #[test]
    fn clipboard_socket_dir_is_none_on_linux_without_absolute_xdg_runtime_dir() {
        for vars in NO_RUNTIME_DIR {
            let dir = clipboard_socket_dir_from(&env(vars), false, 1000);
            assert_eq!(dir, None, "{vars:?}");
        }
    }

    #[test]
    fn macos_clipboard_socket_dir_is_a_tmp_dir_per_user_that_fits_long_sandbox_names() {
        let dir = clipboard_socket_dir_from(&env(&[]), true, u32::MAX).unwrap();
        let socket = dir.join(format!("{}.sock", "a".repeat(63)));

        assert_eq!(
            clipboard_socket_dir_from(&env(&[]), true, 501),
            Some(PathBuf::from("/private/tmp/fbkc-501"))
        );
        assert!(socket.as_os_str().len() < 104, "{}", socket.display());
    }

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

    #[test]
    fn network_log_dir_is_under_the_log_dir() {
        let cases = [
            (
                &[("XDG_STATE_HOME", "/state")][..],
                Some("/state/firebrick/network"),
            ),
            (
                &[("HOME", "/home/user")],
                Some("/home/user/.local/state/firebrick/network"),
            ),
            (&[("HOME", "home")], None),
        ];
        for (vars, expected) in cases {
            assert_eq!(
                network_log_dir_from(&env(vars)),
                expected.map(PathBuf::from),
                "{vars:?}"
            );
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

    #[test]
    fn strip_control_keeps_plain_multi_line_text() {
        let text = "error: no such version\n\tat mise.toml line 2\n";

        assert_eq!(strip_control(text), text);
    }

    #[test]
    fn strip_control_removes_c0_controls() {
        assert_eq!(strip_control("a\rb\x00c\x08"), "abc");
    }

    #[test]
    fn strip_control_removes_del() {
        assert_eq!(strip_control("a\x7fb"), "ab");
    }

    #[test]
    fn strip_control_removes_csi_sequences() {
        assert_eq!(strip_control("\x1b[1;32mok\x1b[0m"), "ok");
        assert_eq!(strip_control("a\x1b[?25lb"), "ab");
    }

    #[test]
    fn strip_control_removes_osc_sequences() {
        assert_eq!(strip_control("a\x1b]52;c;eA==\x07b"), "ab");
        assert_eq!(strip_control("a\x1b]0;title\x1b\\b"), "ab");
    }

    #[test]
    fn strip_control_removes_two_character_escapes() {
        assert_eq!(strip_control("a\x1bcb\x1b7\x1b(Bc"), "abc");
    }

    #[test]
    fn strip_control_removes_c1_sequences() {
        assert_eq!(
            strip_control("a\u{9b}31mb\u{9d}52;c;eA==\u{9c}c\u{85}d"),
            "abcd"
        );
    }

    #[test]
    fn strip_control_drops_unterminated_sequences_up_to_the_end() {
        let cases = [
            ("a\x1b]52;c;eA==", "a"),
            ("a\x1b]x\nb", "a"),
            ("a\x1b[31", "a"),
            ("a\x1b", "a"),
        ];
        for (text, expected) in cases {
            assert_eq!(strip_control(text), expected, "{text:?}");
        }
    }

    #[test]
    fn strip_control_leaves_no_control_characters() {
        let parts = [
            "a", "\x1b", "[", "]", "\x07", "\\", "1;", "(", " ", "\n", "\t", "\r", "\x00", "\x7f",
            "\u{85}", "\u{9b}", "\u{9c}", "\u{9d}",
        ];
        let mut texts = vec![String::new()];
        for _ in 0..3 {
            texts = texts
                .iter()
                .flat_map(|text| parts.map(|part| format!("{text}{part}")))
                .collect();
        }
        for text in texts {
            let stripped = strip_control(&text);
            assert!(
                !stripped
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\t')),
                "{text:?} -> {stripped:?}"
            );
        }
    }
}
