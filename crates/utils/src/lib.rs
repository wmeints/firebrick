//! File locations, naming rules and text helpers shared by the Firebrick CLI and daemon.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
/// to: `$XDG_RUNTIME_DIR/firebrick/netlog`.
///
/// Returns `None` when `XDG_RUNTIME_DIR` isn't an absolute path. Unlike [`socket_path`], it
/// never falls back to the temp directory, where another user could create the directory
/// first and take over the sockets.
pub fn netlog_socket_dir() -> Option<PathBuf> {
    netlog_socket_dir_from(&env_var)
}

/// Returns the network activity socket directory for the environment that `var` describes.
fn netlog_socket_dir_from(var: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    absolute_var(var, "XDG_RUNTIME_DIR").map(|dir| dir.join("firebrick/netlog"))
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
/// clipboard. This removes whole CSI and OSC sequences, in their `ESC` and C1 forms, and other
/// `ESC` sequences, then the remaining C0 controls, DEL and C1 controls, and keeps the text
/// around them. A sequence that is cut off by the end of the text or by an unexpected character
/// only loses its introducer, so the rest of the text shows.
pub fn strip_control(text: &str) -> String {
    let mut stripper = Stripper::default();
    text.chars().for_each(|c| stripper.push(c));
    stripper.finish()
}

/// The part of a terminal escape sequence that [`Stripper`] is in.
#[derive(Default)]
enum Sequence {
    /// Plain text, outside a sequence.
    #[default]
    None,
    /// After `ESC`, before its final character.
    Escape,
    /// Inside the parameters of a CSI sequence.
    Csi,
    /// Inside the string of an OSC sequence.
    Osc,
}

/// Removes terminal escape sequences and control characters from text one character at a time.
#[derive(Default)]
struct Stripper {
    /// The text without sequences and controls so far.
    output: String,
    /// The sequence the last character was part of.
    sequence: Sequence,
    /// The characters after the introducer of the current sequence, which are kept when the
    /// sequence turns out to be cut off.
    pending: String,
}

impl Stripper {
    /// Adds the next character of the text.
    fn push(&mut self, c: char) {
        match self.sequence {
            Sequence::None => self.push_text(c),
            Sequence::Escape => self.push_escape(c),
            Sequence::Csi => self.push_csi(c),
            Sequence::Osc => self.push_osc(c),
        }
    }

    /// Adds a character outside a sequence, starting a sequence at `ESC`, CSI or OSC.
    fn push_text(&mut self, c: char) {
        match c {
            '\x1b' => self.sequence = Sequence::Escape,
            '\u{9b}' => self.sequence = Sequence::Csi,
            '\u{9d}' => self.sequence = Sequence::Osc,
            '\n' | '\t' => self.output.push(c),
            c if c.is_control() => {}
            c => self.output.push(c),
        }
    }

    /// Adds a character after `ESC`: intermediates `0x20`–`0x2F`, then a final character
    /// `0x30`–`0x7E`, where `[` and `]` start a CSI or OSC sequence.
    fn push_escape(&mut self, c: char) {
        match c {
            ' '..='/' => self.pending.push(c),
            '[' if self.pending.is_empty() => self.sequence = Sequence::Csi,
            ']' if self.pending.is_empty() => self.sequence = Sequence::Osc,
            '0'..='~' => self.end(),
            c => self.cut_off(c),
        }
    }

    /// Adds a character of a CSI sequence: parameters and intermediates `0x20`–`0x3F`, then a
    /// final character `0x40`–`0x7E`.
    fn push_csi(&mut self, c: char) {
        match c {
            ' '..='?' => self.pending.push(c),
            '@'..='~' => self.end(),
            c => self.cut_off(c),
        }
    }

    /// Adds a character of an OSC string, which ends at BEL, ST or the `ESC` of `ESC \`.
    fn push_osc(&mut self, c: char) {
        match c {
            '\x07' | '\u{9c}' => self.end(),
            '\x1b' => {
                self.end();
                self.sequence = Sequence::Escape;
            }
            c if c.is_control() => self.cut_off(c),
            c => self.pending.push(c),
        }
    }

    /// Ends the current sequence, dropping it.
    fn end(&mut self) {
        self.sequence = Sequence::None;
        self.pending.clear();
    }

    /// Ends a sequence that `c` cut off, keeping the characters after its introducer, and adds
    /// `c` as text.
    fn cut_off(&mut self, c: char) {
        self.keep_pending();
        self.push_text(c);
    }

    /// Ends the current sequence, keeping the characters after its introducer.
    fn keep_pending(&mut self) {
        self.sequence = Sequence::None;
        self.output.push_str(&self.pending);
        self.pending.clear();
    }

    /// Returns the text, keeping the rest of a sequence that the end of the text cut off.
    fn finish(mut self) -> String {
        self.keep_pending();
        self.output
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

    #[test]
    fn netlog_socket_dir_is_under_xdg_runtime_dir() {
        let var = env(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        assert_eq!(
            netlog_socket_dir_from(&var),
            Some(PathBuf::from("/run/user/1000/firebrick/netlog"))
        );
    }

    #[test]
    fn netlog_socket_dir_is_none_without_absolute_xdg_runtime_dir() {
        for vars in [
            &[][..],
            &[("XDG_RUNTIME_DIR", "run")],
            &[("XDG_RUNTIME_DIR", "")],
        ] {
            assert_eq!(netlog_socket_dir_from(&env(vars)), None, "{vars:?}");
        }
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
        assert_eq!(strip_control("a\x1b[?25lb\x1b[2 qc"), "abc");
    }

    #[test]
    fn strip_control_removes_osc_sequences() {
        assert_eq!(strip_control("a\x1b]52;c;eA==\x07b"), "ab");
        assert_eq!(strip_control("a\x1b]0;title\x1b\\b"), "ab");
    }

    #[test]
    fn strip_control_removes_two_character_escapes() {
        assert_eq!(strip_control("a\x1bcb\x1b7c\x1b(Bd"), "abcd");
    }

    #[test]
    fn strip_control_removes_c1_sequences() {
        assert_eq!(
            strip_control("a\u{9b}31mb\u{9d}52;c;eA==\u{9c}c\u{85}d"),
            "abcd"
        );
    }

    #[test]
    fn strip_control_keeps_the_rest_of_unterminated_sequences() {
        let cases = [
            ("a\x1b]52;c;eA==", "a52;c;eA=="),
            ("a\x1b[31", "a31"),
            ("a\x1b", "a"),
            ("a\x1b]x\nb", "ax\nb"),
            ("a\x1b[1\x1b[0mb", "a1b"),
            ("a\x1b\x00\u{9b}é", "aé"),
        ];
        for (text, expected) in cases {
            assert_eq!(strip_control(text), expected, "{text:?}");
        }
    }

    #[test]
    fn strip_control_leaves_no_control_characters() {
        let parts = [
            "a", "\x1b", "[", "]", "\x07", "\\", "1;", "\n", "\u{9b}", "\u{9c}", "\u{9d}",
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
                !stripped.chars().any(|c| c.is_control() && c != '\n'),
                "{text:?} -> {stripped:?}"
            );
        }
    }
}
