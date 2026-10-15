//! File locations, local endpoints, naming rules and text helpers shared by the Firebrick CLI
//! and daemon.

pub mod ipc;
#[cfg(windows)]
mod win32;

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Name used when a path's leaf has no ASCII letters or digits left after sanitizing.
const FALLBACK_LABEL: &str = "sandbox";

/// Error for when neither `XDG_DATA_HOME` nor `HOME` is an absolute path.
#[cfg(unix)]
pub const NO_DATA_DIR: &str =
    "can't choose a data directory: set HOME or XDG_DATA_HOME to an absolute path";

/// Error for when neither `XDG_DATA_HOME` nor `LOCALAPPDATA` is an absolute path.
#[cfg(windows)]
pub const NO_DATA_DIR: &str =
    "can't choose a data directory: set LOCALAPPDATA or XDG_DATA_HOME to an absolute path";

/// Error for when neither `XDG_STATE_HOME` nor `HOME` is an absolute path.
#[cfg(unix)]
pub const NO_LOG_DIR: &str =
    "can't choose a log directory: set HOME or XDG_STATE_HOME to an absolute path";

/// Error for when neither `XDG_STATE_HOME` nor `LOCALAPPDATA` is an absolute path.
#[cfg(windows)]
pub const NO_LOG_DIR: &str =
    "can't choose a log directory: set LOCALAPPDATA or XDG_STATE_HOME to an absolute path";

/// Error for when neither `XDG_CONFIG_HOME` nor `HOME` is an absolute path.
#[cfg(unix)]
pub const NO_CONFIG_DIR: &str =
    "can't find the firebrick config directory: set HOME or XDG_CONFIG_HOME to an absolute path";

/// Error for when neither `XDG_CONFIG_HOME` nor `APPDATA` is an absolute path.
#[cfg(windows)]
pub const NO_CONFIG_DIR: &str = "can't find the firebrick config directory: set APPDATA or \
                                 XDG_CONFIG_HOME to an absolute path";

/// The environment variable that holds the user's home directory, which `~` in a mount and
/// `~/.ssh` resolve against.
#[cfg(unix)]
pub const HOME_VAR: &str = "HOME";

/// The environment variable that holds the user's home directory, which `~` in a mount and
/// `~/.ssh` resolve against.
#[cfg(windows)]
pub const HOME_VAR: &str = "USERPROFILE";

/// Returns the daemon's endpoint (see [`ipc`]): the unix socket `$XDG_RUNTIME_DIR/fbkd.sock`,
/// falling back to the temp directory. It never fails on Unix.
#[cfg(unix)]
pub fn socket_path() -> io::Result<PathBuf> {
    Ok(runtime_dir_from(&env_var).join("fbkd.sock"))
}

/// Returns the daemon's endpoint (see [`ipc`]): the named pipe `<prefix>-fbkd`, where the prefix
/// is `\\.\pipe\firebrick-<SID of the user>` unless `FIREBRICK_PIPE_PREFIX` names another pipe.
/// The name is predictable, so it's [`ipc`] that keeps other users from serving or using it.
/// Fails when the user's SID can't be read or `FIREBRICK_PIPE_PREFIX` isn't a local pipe name,
/// instead of falling back to a name that every user shares.
#[cfg(windows)]
pub fn socket_path() -> io::Result<PathBuf> {
    Ok(with_suffix(pipe_prefix()?, "-fbkd"))
}

/// Returns the directory of the endpoints that sandboxes report their network activity to (see
/// [`ipc::endpoint`]): `$XDG_RUNTIME_DIR/firebrick/netlog`, falling back to
/// `/private/tmp/fbk-<uid>` on macOS, which has no `XDG_RUNTIME_DIR`, and the pipe name
/// `<prefix>-netlog` on Windows, with the prefix of [`socket_path`].
///
/// Returns `None` on Linux when `XDG_RUNTIME_DIR` isn't an absolute path. Unlike
/// [`socket_path`], it never falls back to the temp directory, whose path on macOS is too long
/// for the sockets of long sandbox names. The macOS directory is in the shared `/tmp`, where
/// another user could create it first, so check it with [`ipc::prepare_dir`] before use. Fails
/// on Windows like [`socket_path`].
pub fn netlog_socket_dir() -> io::Result<Option<PathBuf>> {
    socket_dir(NETLOG_SOCKETS)
}

/// Returns the directory of the endpoints that sandboxes read the host clipboard through (see
/// [`ipc::endpoint`]): `$XDG_RUNTIME_DIR/firebrick/clipboard`, falling back to
/// `/private/tmp/fbkc-<uid>` on macOS, which has no `XDG_RUNTIME_DIR`, and the pipe name
/// `<prefix>-clipboard` on Windows.
///
/// Like [`netlog_socket_dir`], it never falls back to the temp directory and returns `None` on
/// Linux without an absolute `XDG_RUNTIME_DIR`. The macOS directory sits next to the network
/// log's instead of inside it, so the sockets of 63-character sandbox names still fit in a unix
/// socket address. Check it with [`ipc::prepare_dir`] before use. Fails on Windows like
/// [`socket_path`].
pub fn clipboard_socket_dir() -> io::Result<Option<PathBuf>> {
    socket_dir(CLIPBOARD_SOCKETS)
}

/// Where the endpoints of one vsock channel live.
#[derive(Debug, Clone, Copy)]
struct SocketDir {
    /// The directory under `$XDG_RUNTIME_DIR`.
    #[cfg(unix)]
    runtime_subdir: &'static str,
    /// The start of the directory in `/private/tmp` on macOS, followed by the user's UID.
    #[cfg(unix)]
    macos_prefix: &'static str,
    /// What follows the pipe prefix on Windows.
    #[cfg(windows)]
    pipe_suffix: &'static str,
}

/// The endpoints of the network log.
const NETLOG_SOCKETS: SocketDir = SocketDir {
    #[cfg(unix)]
    runtime_subdir: "firebrick/netlog",
    #[cfg(unix)]
    macos_prefix: "fbk-",
    #[cfg(windows)]
    pipe_suffix: "-netlog",
};

/// The endpoints of the clipboard.
const CLIPBOARD_SOCKETS: SocketDir = SocketDir {
    #[cfg(unix)]
    runtime_subdir: "firebrick/clipboard",
    #[cfg(unix)]
    macos_prefix: "fbkc-",
    #[cfg(windows)]
    pipe_suffix: "-clipboard",
};

/// Returns the pipe name of the channel's endpoints.
#[cfg(windows)]
fn socket_dir(sockets: SocketDir) -> io::Result<Option<PathBuf>> {
    Ok(Some(with_suffix(pipe_prefix()?, sockets.pipe_suffix)))
}

/// The start of every local pipe name.
#[cfg(windows)]
const PIPES: &str = r"\\.\pipe\";

/// Returns the start of the names of firebrick's pipes for the current user.
#[cfg(windows)]
fn pipe_prefix() -> io::Result<PathBuf> {
    pipe_prefix_from(&env_var, || {
        let sid = win32::Sid::current_user().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("can't read your user's SID to name firebrick's pipes: {err}"),
            )
        })?;
        sid.string()
    })
}

/// Returns the start of the pipe names for the environment that `var` describes: the value of
/// `FIREBRICK_PIPE_PREFIX`, which isolates test daemons, or `\\.\pipe\firebrick-<user>` with
/// the SID string that `user` returns. Fails when `FIREBRICK_PIPE_PREFIX` is set but isn't a
/// local pipe name, so a typo never sends `fbk` to the user's real daemon.
#[cfg(windows)]
fn pipe_prefix_from(
    var: &impl Fn(&str) -> Option<OsString>,
    user: impl FnOnce() -> io::Result<String>,
) -> io::Result<PathBuf> {
    let Some(prefix) = var("FIREBRICK_PIPE_PREFIX").filter(|prefix| !prefix.is_empty()) else {
        return Ok(PathBuf::from(format!("{PIPES}firebrick-{}", user()?)));
    };

    let name = prefix.to_string_lossy();
    let is_pipe = name
        .get(..PIPES.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(PIPES))
        && name.len() > PIPES.len();
    if !is_pipe {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "FIREBRICK_PIPE_PREFIX must be a local pipe name such as {PIPES}fbk-test, not {name}"
            ),
        ));
    }

    Ok(PathBuf::from(prefix))
}

/// Returns the pipe name `name` with `suffix` appended.
#[cfg(windows)]
fn with_suffix(name: PathBuf, suffix: &str) -> PathBuf {
    let mut name = name.into_os_string();
    name.push(suffix);
    name.into()
}

/// Returns the socket directory of the channel in fbkd's environment.
#[cfg(unix)]
fn socket_dir(sockets: SocketDir) -> io::Result<Option<PathBuf>> {
    let uid = nix::unistd::geteuid().as_raw();
    Ok(socket_dir_from(
        &env_var,
        cfg!(target_os = "macos"),
        uid,
        sockets,
    ))
}

/// Returns the socket directory of the channel for the environment that `var` describes, on
/// macOS when `macos` is set, for the user `uid`. The macOS directory is canonical, because
/// microsandbox refuses host paths that go through a symlink, such as `/tmp`.
#[cfg(unix)]
fn socket_dir_from(
    var: &impl Fn(&str) -> Option<OsString>,
    macos: bool,
    uid: u32,
    sockets: SocketDir,
) -> Option<PathBuf> {
    absolute_var(var, "XDG_RUNTIME_DIR")
        .map(|dir| dir.join(sockets.runtime_subdir))
        .or_else(|| macos.then(|| format!("/private/tmp/{}{uid}", sockets.macos_prefix).into()))
}

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

/// Returns `$XDG_RUNTIME_DIR`, falling back to the temp directory, for the environment that
/// `var` describes.
#[cfg(unix)]
fn runtime_dir_from(var: &impl Fn(&str) -> Option<OsString>) -> PathBuf {
    var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Returns the directory the daemon records the network activity of sandboxes in:
/// `$XDG_STATE_HOME/firebrick/network`, falling back to `$HOME/.local/state/firebrick/network`,
/// or `%LOCALAPPDATA%\firebrick\state\network` on Windows.
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
/// for example by reading the other process's memory. [`ipc`] applies it on Unix.
#[cfg(unix)]
pub fn is_trusted_peer(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid || peer_uid == 0
}

/// Returns the directory the daemon writes its log files to: `$XDG_STATE_HOME/firebrick`,
/// falling back to `$HOME/.local/state/firebrick`, or `%LOCALAPPDATA%\firebrick\state` on Windows.
///
/// Returns `None` when neither variable is an absolute path, so the logs never end up in a
/// temp directory that other users can create first.
pub fn log_dir() -> Option<PathBuf> {
    log_dir_from(&env_var)
}

/// Returns the log directory for the environment that `var` describes.
fn log_dir_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    firebrick_dir(var, STATE)
}

/// Returns the microsandbox home the daemon uses when `MSB_HOME` isn't set, so it never shares
/// the runtime, database and sandboxes of a separately installed `msb` in `~/.microsandbox`:
/// `$XDG_STATE_HOME/firebrick/msb`, falling back to `$HOME/.local/state/firebrick/msb`, or
/// `%LOCALAPPDATA%\firebrick\state\msb` on Windows.
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

/// Where one kind of firebrick directory lives.
#[derive(Debug, Clone, Copy)]
struct BaseDir {
    /// The XDG base directory variable, which wins on every platform.
    xdg_var: &'static str,
    /// The XDG base directory's default under `$HOME`.
    #[cfg(unix)]
    home_subdir: &'static str,
    /// The known folder variable that holds the directory on Windows.
    #[cfg(windows)]
    windows_var: &'static str,
    /// The firebrick directory under the known folder.
    #[cfg(windows)]
    windows_subdir: &'static str,
}

/// The daemon's persistent data: the secrets and SSH keys.
const DATA: BaseDir = BaseDir {
    xdg_var: "XDG_DATA_HOME",
    #[cfg(unix)]
    home_subdir: ".local/share",
    #[cfg(windows)]
    windows_var: "LOCALAPPDATA",
    #[cfg(windows)]
    windows_subdir: r"firebrick\data",
};

/// The daemon's logs, network records and microsandbox home.
const STATE: BaseDir = BaseDir {
    xdg_var: "XDG_STATE_HOME",
    #[cfg(unix)]
    home_subdir: ".local/state",
    #[cfg(windows)]
    windows_var: "LOCALAPPDATA",
    #[cfg(windows)]
    windows_subdir: r"firebrick\state",
};

/// The CLI's approvals.
const CONFIG: BaseDir = BaseDir {
    xdg_var: "XDG_CONFIG_HOME",
    #[cfg(unix)]
    home_subdir: ".config",
    #[cfg(windows)]
    windows_var: "APPDATA",
    #[cfg(windows)]
    windows_subdir: "firebrick",
};

/// Returns the firebrick directory of the kind `base`, with environment variables looked up
/// through `var`: `firebrick` in the XDG base directory, falling back to the platform's default
/// (see [`platform_dir`]). Empty and relative values are ignored, as the XDG Base Directory
/// spec requires.
fn firebrick_dir(var: &impl Fn(&str) -> Option<OsString>, base: BaseDir) -> Option<PathBuf> {
    absolute_var(var, base.xdg_var)
        .map(|dir| dir.join("firebrick"))
        .or_else(|| platform_dir(var, base))
}

/// Returns the firebrick directory under the XDG base directory's default in `$HOME`.
#[cfg(unix)]
fn platform_dir(var: &impl Fn(&str) -> Option<OsString>, base: BaseDir) -> Option<PathBuf> {
    absolute_var(var, "HOME").map(|home| home.join(base.home_subdir).join("firebrick"))
}

/// Returns the firebrick directory under the known folder, such as `%LOCALAPPDATA%`. Ignores
/// `HOME`, which Git Bash sets, so `fbk` finds the same daemon from every shell.
#[cfg(windows)]
fn platform_dir(var: &impl Fn(&str) -> Option<OsString>, base: BaseDir) -> Option<PathBuf> {
    absolute_var(var, base.windows_var).map(|dir| dir.join(base.windows_subdir))
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
/// `$HOME/.config/firebrick/approved.yml`, or `%APPDATA%\firebrick\approved.yml` on Windows.
///
/// Returns `None` when neither variable is an absolute path. Sandboxes mount the workspace, so
/// the file must never end up in the current directory or a temp directory, where an agent
/// could write it.
pub fn approvals_path() -> Option<PathBuf> {
    approvals_path_from(&env_var)
}

/// Returns the approvals file for the environment that `var` describes.
fn approvals_path_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    firebrick_dir(var, CONFIG).map(|config| config.join("approved.yml"))
}

/// Returns the directory holding the daemon's persistent data: `$XDG_DATA_HOME/firebrick`,
/// falling back to `$HOME/.local/share/firebrick`, or `%LOCALAPPDATA%\firebrick\data` on Windows.
///
/// Returns `None` when neither variable is an absolute path, so the secrets and SSH keys never
/// end up in a temp directory that other users can create first.
fn data_dir() -> Option<PathBuf> {
    data_dir_from(&env_var)
}

/// Returns the data directory for the environment that `var` describes.
fn data_dir_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    firebrick_dir(var, DATA)
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
    pub(crate) fn env<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(value))
        }
    }

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
/// The locations and sockets on Unix.
#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use crate::tests::env;

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
                socket_dir_from(&var, macos, 1000, NETLOG_SOCKETS),
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
            let dir = socket_dir_from(&env(vars), false, 1000, NETLOG_SOCKETS);
            assert_eq!(dir, None, "{vars:?}");
        }
    }

    #[test]
    fn netlog_socket_dir_falls_back_to_a_canonical_tmp_dir_per_user_on_macos() {
        for vars in NO_RUNTIME_DIR {
            assert_eq!(
                socket_dir_from(&env(vars), true, 501, NETLOG_SOCKETS),
                Some(PathBuf::from("/private/tmp/fbk-501")),
                "{vars:?}"
            );
        }
    }

    #[test]
    fn macos_netlog_socket_dir_fits_sockets_of_63_character_sandbox_names() {
        let dir = socket_dir_from(&env(&[]), true, u32::MAX, NETLOG_SOCKETS).unwrap();
        let socket = dir.join(format!("{}.sock", "a".repeat(63)));

        assert!(socket.as_os_str().len() < 104, "{}", socket.display());
    }

    #[test]
    fn clipboard_sockets_have_their_own_directory_that_fits_long_sandbox_names() {
        let var = env(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        let macos = socket_dir_from(&env(&[]), true, u32::MAX, CLIPBOARD_SOCKETS).unwrap();
        let socket = macos.join(format!("{}.sock", "a".repeat(63)));

        assert_eq!(
            socket_dir_from(&var, false, 1000, CLIPBOARD_SOCKETS),
            Some(PathBuf::from("/run/user/1000/firebrick/clipboard"))
        );
        assert_eq!(
            macos,
            PathBuf::from(format!("/private/tmp/fbkc-{}", u32::MAX))
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
}

/// The locations on Windows, where `HOME` doesn't count and the known folders do.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::tests::env;

    /// Returns the access list of the path in Security Descriptor Definition Language, as
    /// `icacls /save` writes it, which doesn't depend on the display language.
    fn sddl(path: &Path) -> String {
        let out = tempfile::TempDir::new().unwrap();
        let saved = out.path().join("acl");
        let status = std::process::Command::new("icacls")
            .arg(path)
            .arg("/save")
            .arg(&saved)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = fs::read(saved).unwrap();
        let wide: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&wide)
    }

    /// Asserts that the access list grants access to the user, the system (`SY`) and the
    /// administrators (`BA`) only, and to nobody else, such as the built-in users or everyone.
    fn assert_private(acl: &str) {
        assert_trustees(acl, &["BA", "SY"]);
    }

    /// Asserts that the access list in Security Descriptor Definition Language grants access to
    /// the user and the `others` only, in any order.
    pub(crate) fn assert_trustees(acl: &str, others: &[&str]) {
        let user = win32::Sid::current_user().unwrap().string().unwrap();
        // The trustee is the last field of each `(A;flags;rights;;;trustee)` entry.
        let mut trustees: Vec<&str> = acl
            .split('(')
            .skip(1)
            .filter_map(|entry| entry.split(')').next()?.rsplit(';').next())
            .collect();
        trustees.sort_unstable();

        let mut expected = others.to_vec();
        expected.push(user_trustee(&user));
        expected.sort_unstable();
        assert_eq!(trustees, expected, "{acl}");
    }

    /// Returns how SDDL names the user as a trustee: by SID, or as `LA` for the built-in local
    /// administrator account (RID 500), which CI runners use.
    fn user_trustee(user: &str) -> &str {
        if user.ends_with("-500") { "LA" } else { user }
    }

    /// Returns a temp directory outside the user's profile, in `C:\`, whose access list it
    /// inherits, like a directory that `XDG_DATA_HOME` points to outside the profile. The
    /// profile's temp directory is private already, so it would hide a missing restriction.
    fn outside_profile() -> tempfile::TempDir {
        tempfile::TempDir::new_in(r"C:\").unwrap()
    }

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

    /// Returns the data, log and microsandbox directories for an environment holding `vars`.
    fn dirs(vars: &[(&str, &str)]) -> [Option<PathBuf>; 3] {
        let var = env(vars);
        [data_dir_from(&var), log_dir_from(&var), msb_home_from(&var)]
    }

    #[test]
    fn dirs_are_under_xdg_dirs() {
        let vars = [
            ("XDG_DATA_HOME", r"C:\data"),
            ("XDG_STATE_HOME", r"C:\state"),
            ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
        ];
        let expected = [
            r"C:\data\firebrick",
            r"C:\state\firebrick",
            r"C:\state\firebrick\msb",
        ];
        assert_eq!(dirs(&vars), expected.map(|dir| Some(PathBuf::from(dir))));
    }

    #[test]
    fn dirs_fall_back_to_local_app_data_and_ignore_home() {
        let vars = [
            ("HOME", r"C:\Users\me"),
            ("XDG_DATA_HOME", "data"),
            ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
        ];
        let expected = [
            r"C:\Users\me\AppData\Local\firebrick\data",
            r"C:\Users\me\AppData\Local\firebrick\state",
            r"C:\Users\me\AppData\Local\firebrick\state\msb",
        ];
        assert_eq!(dirs(&vars), expected.map(|dir| Some(PathBuf::from(dir))));
    }

    #[test]
    fn dirs_are_none_without_absolute_local_app_data() {
        for vars in [
            &[("HOME", r"C:\Users\me")][..],
            &[("LOCALAPPDATA", "Local")],
        ] {
            assert_eq!(dirs(vars), [None, None, None], "{vars:?}");
            assert_eq!(missing_dir_from(&env(vars)), Some(NO_DATA_DIR), "{vars:?}");
        }
    }

    #[test]
    fn approvals_path_falls_back_to_app_data() {
        let vars = [
            ("HOME", r"C:\Users\me"),
            ("APPDATA", r"C:\Users\me\AppData\Roaming"),
        ];
        assert_eq!(
            approvals_path_from(&env(&vars)),
            Some(PathBuf::from(
                r"C:\Users\me\AppData\Roaming\firebrick\approved.yml"
            ))
        );
        assert_eq!(approvals_path_from(&env(&[("HOME", r"C:\Users\me")])), None);
    }

    /// Returns the SID string `S-1-5-21-1`, standing in for the current user.
    fn user() -> io::Result<String> {
        Ok("S-1-5-21-1".to_string())
    }

    #[test]
    fn pipes_are_named_after_the_user() {
        assert_eq!(
            pipe_prefix_from(&env(&[]), user).unwrap(),
            PathBuf::from(r"\\.\pipe\firebrick-S-1-5-21-1")
        );
    }

    #[test]
    fn pipe_prefix_fails_without_a_user_instead_of_sharing_a_name() {
        let no_user = || Err(io::Error::from(io::ErrorKind::PermissionDenied));

        let err = pipe_prefix_from(&env(&[]), no_user).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn pipe_prefix_can_be_overridden_with_another_local_pipe() {
        let isolated = env(&[("FIREBRICK_PIPE_PREFIX", r"\\.\PIPE\fbk-test")]);

        assert_eq!(
            pipe_prefix_from(&isolated, user).unwrap(),
            PathBuf::from(r"\\.\PIPE\fbk-test")
        );
        let unset = env(&[("FIREBRICK_PIPE_PREFIX", "")]);
        assert_eq!(
            pipe_prefix_from(&unset, user).unwrap(),
            PathBuf::from(r"\\.\pipe\firebrick-S-1-5-21-1")
        );
    }

    #[test]
    fn pipe_prefix_refuses_an_override_that_is_no_local_pipe() {
        for prefix in [r"C:\fbk", r"\\server\pipe\fbk", r"\\.\pipe\", "fbk-smoke"] {
            let vars = [("FIREBRICK_PIPE_PREFIX", prefix)];

            let err = pipe_prefix_from(&env(&vars), user).unwrap_err();

            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{prefix:?}");
            assert!(err.to_string().contains(prefix), "{err}");
        }
    }

    #[test]
    fn the_daemon_and_each_channel_have_a_pipe_of_the_current_user() {
        let daemon = socket_path().unwrap().to_string_lossy().into_owned();
        let netlog = netlog_socket_dir().unwrap().unwrap();
        let clipboard = clipboard_socket_dir().unwrap().unwrap();
        let prefix = daemon.strip_suffix("-fbkd").unwrap();

        // Local and domain accounts have `S-1-5-` SIDs, Entra ID accounts `S-1-12-1-`.
        assert!(prefix.starts_with(r"\\.\pipe\firebrick-S-1-"), "{daemon}");
        assert_eq!(netlog, PathBuf::from(format!("{prefix}-netlog")));
        assert_eq!(clipboard, PathBuf::from(format!("{prefix}-clipboard")));
    }
}
